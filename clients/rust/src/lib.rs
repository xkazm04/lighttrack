//! LightTrack Rust client — fire-and-forget LLM event ingestion.
//!
//! Reuses [`lighttrack_core::LlmEvent`] as the wire type, so the payload can never drift from the
//! API. Sends are best-effort and non-blocking: events go to a background worker thread over a
//! channel, which POSTs them. The worker drains and joins when the [`Client`] is dropped (or on an
//! explicit [`Client::flush`]).
//!
//! Best-effort does not mean silent: a send that fails writes one actionable, rate-limited line to
//! stderr (see [`Client::quiet`] / `LIGHTTRACK_QUIET=1` to turn that off). It still never panics.
//!
//! ```no_run
//! use lighttrack_client::Client;
//! let lt = Client::from_env();
//! lt.event("openai", "gpt-4o")
//!     .input_tokens(120).output_tokens(45).latency_ms(210)
//!     .send();
//! lt.flush(); // drain the background worker before exit
//! ```

mod admission;
mod diagnostics;
mod extract;
mod limits;
mod pii;

pub use admission::{
    view_from_statuses, Admit, AdmissionCache, AdmitReason, BudgetExceeded, Enforce,
    DEFAULT_ADMISSION_TTL_MS,
};
pub use diagnostics::{diagnostic_kind, no_project_message, send_failure_message, FailureContext};
pub use extract::{extract_anthropic, extract_gemini, extract_openai, Extracted};
pub use limits::{parse_limit_view, BindingScope, LimitView};
pub use lighttrack_core::shed_ticket;
pub use pii::{pii_kinds, PiiRule};

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;

use lighttrack_core::{LlmEvent, TokenUsage};
pub use lighttrack_core::{Operation, Provider, ProviderId, Status};

use diagnostics::Diagnostics;

const DEFAULT_URL: &str = "http://127.0.0.1:8787";

/// A best-effort, non-blocking ingestion client. Cheap to construct; events are POSTed from a
/// background thread. Configure via [`Client::from_env`] or [`Client::new`].
pub struct Client {
    base_url: String,
    project: Option<String>,
    source: Option<String>,
    /// Whether an API key was configured. The key itself lives in the worker thread; only this
    /// answer is needed here, to tell a first-run misconfiguration from a legitimately keyed client.
    has_key: bool,
    diag: Arc<Diagnostics>,
    tx: Option<Sender<(&'static str, Value)>>,
    worker: Option<JoinHandle<()>>,
    /// What the server last said about this project's caps. Written by the worker thread as
    /// responses land, read by [`Client::admit`] on the caller's thread.
    limits: Arc<Mutex<AdmissionCache>>,
    enforce: Enforce,
    record_blocked: bool,
    /// A second handle on the key, for the one call this side of the channel makes:
    /// [`Client::refresh_limits`] reads `GET /v1/limits/status`, which the worker (a write-only
    /// pipe) cannot issue. `has_key` above stays the cheap question the send path asks.
    api_key: Option<String>,
}

impl Client {
    /// Build from `LIGHTTRACK_URL`, `LIGHTTRACK_KEY`, `LIGHTTRACK_PROJECT`.
    pub fn from_env() -> Self {
        Self::new(
            std::env::var("LIGHTTRACK_URL").unwrap_or_else(|_| DEFAULT_URL.to_string()),
            std::env::var("LIGHTTRACK_KEY")
                .ok()
                .filter(|s| !s.is_empty()),
            std::env::var("LIGHTTRACK_PROJECT")
                .ok()
                .filter(|s| !s.is_empty()),
        )
    }

    /// A project key derives the project server-side; set `project` only for dev mode (no key) or an
    /// admin key ingesting into a specific project.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        project: Option<String>,
    ) -> Self {
        let base = base_url.into().trim_end_matches('/').to_string();
        let has_key = api_key.is_some();
        let diag = Arc::new(Diagnostics::from_env());
        let (tx, rx) = mpsc::channel::<(&'static str, Value)>();
        let worker_diag = Arc::clone(&diag);
        let worker_base = base.clone();
        let limits = Arc::new(Mutex::new(AdmissionCache::default()));
        let worker_limits = Arc::clone(&limits);
        let key_for_reads = api_key.clone();
        let worker = std::thread::Builder::new()
            .name("lighttrack".into())
            .spawn(move || {
                let http = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(2))
                    .build()
                    .unwrap_or_else(|_| reqwest::blocking::Client::new());
                // Receives (path, body) until all senders drop; delivers queued items first, so Drop
                // drains. `path` is /v1/events for calls and /v1/scores for guard verdicts.
                while let Ok((path, body)) = rx.recv() {
                    let mut req = http.post(format!("{worker_base}{path}")).json(&body);
                    if let Some(k) = &api_key {
                        req = req.bearer_auth(k);
                    }
                    let outcome = SendOutcome::of(req.send());
                    // Every ingest response, accepted or refused, is evidence about the project's
                    // position. Folding it in here is what makes `admit()` answer from the wall the
                    // app is actually near, rather than from a poll it never makes.
                    outcome.observe_into(&worker_limits);
                    // Best-effort: the outcome never propagates to the caller, but it is no longer
                    // discarded either — a rejection or an outage is reported, once per kind.
                    report(&worker_diag, &worker_base, path, &body, has_key, outcome);
                }
            })
            .ok();
        Self {
            base_url: base,
            project,
            source: None,
            has_key,
            diag,
            tx: Some(tx),
            worker,
            limits,
            enforce: std::env::var("LIGHTTRACK_ENFORCE")
                .map(|v| Enforce::from_str_or_off(&v))
                .unwrap_or_default(),
            record_blocked: false,
            api_key: key_for_reads,
        }
    }

    /// Set a `source` label stamped on every event.
    pub fn source(mut self, s: impl Into<String>) -> Self {
        self.source = Some(s.into());
        self
    }

    /// Suppress the stderr diagnostics a dropped or rejected event otherwise reports. Equivalent to
    /// setting `LIGHTTRACK_QUIET=1`; applies to the background worker too.
    pub fn quiet(self, quiet: bool) -> Self {
        self.diag.set_quiet(quiet);
        self
    }

    /// Turn on pre-spend admission (see [`crate::admission`]).
    ///
    /// [`Enforce::Block`] makes [`Client::gate`] refuse a call the project's caps would turn away;
    /// [`Enforce::Warn`] reports it and admits; [`Enforce::Off`] (the default, also read from
    /// `LIGHTTRACK_ENFORCE`) only observes. Off by default deliberately: adding an observability
    /// SDK must not change what an app does.
    pub fn enforce(mut self, mode: Enforce) -> Self {
        self.enforce = mode;
        self
    }

    /// Record a locally-blocked call as a zero-usage event tagged `lt_blocked_locally`.
    ///
    /// A blocked call is *not* spend and is never recorded as spend — but it is traffic the app
    /// attempted, and a rollup that cannot see it reads as a quiet week rather than a throttled one.
    pub fn record_blocked(mut self, on: bool) -> Self {
        self.record_blocked = on;
        self
    }

    /// Would a call be admitted right now? Pure and instant — decided from the last ingest response
    /// this client saw, with no round trip.
    pub fn admit(&self, name: Option<&str>, event_id: Option<&str>) -> Admit {
        let now = chrono::Utc::now().timestamp_millis();
        match self.limits.lock() {
            Ok(c) => c.admit(name, event_id, now),
            // A poisoned lock means a panic mid-update, not a breached budget. Fail open.
            Err(p) => p.into_inner().admit(name, event_id, now),
        }
    }

    /// The enforcement gate: call it immediately before the provider call.
    ///
    /// `Err(BudgetExceeded)` under [`Enforce::Block`]; under [`Enforce::Warn`] it reports and
    /// returns `Ok(())`; under [`Enforce::Off`] it is a no-op. There is no instrumentation wrapper
    /// in this SDK to hide the call inside (Rust provider clients are third-party and
    /// un-monkey-patchable), so the caller invokes it directly:
    ///
    /// ```no_run
    /// # use lighttrack_client::{Client, Enforce};
    /// let lt = Client::from_env().enforce(Enforce::Block);
    /// lt.gate(Some("summarize"))?;   // returns before a token is bought
    /// # Ok::<(), lighttrack_client::BudgetExceeded>(())
    /// ```
    pub fn gate(&self, name: Option<&str>) -> Result<(), BudgetExceeded> {
        if self.enforce == Enforce::Off {
            return Ok(());
        }
        // The server mints the event id, so the client cannot know it in advance: this is a fresh
        // ticket per call. The shed *rate* therefore matches the server's; the shed *set* does not.
        let ticket = lighttrack_core::new_id();
        let verdict = self.admit(name, Some(&ticket));
        if verdict.ok {
            return Ok(());
        }
        let reason = verdict.reason.map(|r| r.as_str()).unwrap_or("unknown");
        if self.record_blocked {
            self.record_blocked_call(name, reason, verdict.retry_after_secs);
        }
        let msg = format!(
            "LightTrack: {} refused before it was made ({reason})",
            name.unwrap_or("this call")
        );
        if self.enforce == Enforce::Warn {
            self.diag
                .warn("budget", &format!("{msg}. enforce=warn, so the call is proceeding anyway."));
            return Ok(());
        }
        Err(BudgetExceeded {
            reason: verdict.reason,
            retry_after_secs: verdict.retry_after_secs,
        })
    }

    /// Record a call this client refused: real traffic, zero usage, and explicitly not spend.
    fn record_blocked_call(&self, name: Option<&str>, reason: &str, retry: Option<u64>) {
        let mut b = self
            .event("lighttrack", "blocked")
            .status(Status::Error)
            .error(format!("blocked locally by pre-spend admission ({reason})"))
            .tag(BLOCKED_TAG)
            .metadata(serde_json::json!({
                "lt_admit_reason": reason,
                "lt_retry_after_secs": retry,
            }));
        if let Some(n) = name {
            b = b.name(n);
        }
        b.send();
    }

    /// Refresh the limit view from `GET /v1/limits/status`. Blocking and best-effort: a failure
    /// leaves the old view in place (fail open). Call it from a background thread, not a hot path.
    pub fn refresh_limits(&self) {
        let url = match &self.project {
            Some(p) => format!("{}/v1/limits/status?project={p}", self.base_url),
            None => format!("{}/v1/limits/status", self.base_url),
        };
        let http = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut req = http.get(url);
        if let Some(k) = &self.api_key {
            req = req.bearer_auth(k);
        }
        let Ok(resp) = req.send() else { return };
        let Ok(body) = resp.json::<Value>() else {
            return;
        };
        if let Some(view) = admission::view_from_statuses(&body["statuses"]) {
            if let Ok(mut c) = self.limits.lock() {
                c.observe(&view, chrono::Utc::now().timestamp_millis());
            }
        }
    }

    /// Start building an event for one LLM call.
    ///
    /// `provider` is any vendor id — `"openai"`, `"anthropic"`, `"mistral"`, `"az.ai.openai"` — not a
    /// closed enum: LightTrack keys prices, limit scopes and rollups on the id you send (M8).
    pub fn event(
        &self,
        provider: impl Into<ProviderId>,
        model: impl Into<String>,
    ) -> EventBuilder<'_> {
        EventBuilder::new(self, provider.into(), model.into())
    }

    /// Low-level: enqueue a fully-built event (best-effort, non-blocking).
    pub fn track(&self, ev: LlmEvent) {
        self.send_raw(
            "/v1/events",
            serde_json::to_value(&ev).unwrap_or(Value::Null),
        );
    }

    /// Enqueue a pre-serialized body to an API path (best-effort, non-blocking).
    fn send_raw(&self, path: &'static str, body: Value) {
        // Catch the misconfiguration that is guaranteed to fail *before* spending a round trip on
        // it: no project and no API key means the server has no way to attribute the event and will
        // answer 400. Checked on the body, so a per-event `.project(...)` override counts.
        if !body_has_project(&body) && !self.has_key {
            self.diag.warn(
                "no-project",
                &diagnostics::no_project_message(&self.base_url),
            );
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send((path, body));
        }
    }

    /// Validate `output` against [`GuardRules`] and record the verdict as a score (best-effort,
    /// non-blocking) so guardrail pass-rates are observable. Returns the verdict so the caller can
    /// act (retry / fallback / block). Never blocks or panics.
    pub fn track_guard(&self, output: &str, rules: &GuardRules, name: Option<&str>) -> GuardResult {
        let result = guard(output, rules);
        let score = lighttrack_core::Score {
            id: lighttrack_core::new_id(),
            project_id: self.project.clone().unwrap_or_default(),
            event_id: None,
            rubric: name
                .map(|n| format!("guard:{n}"))
                .unwrap_or_else(|| "guard".into()),
            // A guard is not a stored rubric: it is an inline, freeform verdict with no id to cite.
            rubric_id: None,
            kind: lighttrack_core::ScoreKind::Freeform,
            value: if result.ok { 1.0 } else { 0.0 },
            max: 1.0,
            pass: Some(result.ok),
            reasoning: Some(if result.violations.is_empty() {
                "all checks passed".to_string()
            } else {
                result.violations.join("; ")
            }),
            // A guard verdict is inline and deterministic: it belongs to no benchmark run and
            // carries no per-dimension breakdown.
            detail: None,
            run_id: None,
            case_index: None,
            scored_by: self
                .source
                .clone()
                .map(|s| format!("guard:{s}"))
                .unwrap_or_else(|| "lighttrack-guard".into()),
            cost_usd: None,
            created_at: chrono::Utc::now(),
        };
        self.send_raw(
            "/v1/scores",
            serde_json::to_value(&score).unwrap_or(Value::Null),
        );
        result
    }

    /// Track from an OpenAI chat/responses JSON value (extracts model + token usage).
    pub fn track_openai_json(&self, resp: &Value, model: Option<&str>) {
        self.track_extracted("openai", extract::extract_openai(resp), model);
    }

    /// Track from an Anthropic messages JSON value.
    pub fn track_anthropic_json(&self, resp: &Value, model: Option<&str>) {
        self.track_extracted("anthropic", extract::extract_anthropic(resp), model);
    }

    /// Track from a Gemini generateContent JSON value (model is usually passed in).
    pub fn track_gemini_json(&self, resp: &Value, model: Option<&str>) {
        self.track_extracted("google", extract::extract_gemini(resp), model);
    }

    /// Send what an extractor read. An explicit `model` wins over the response's own — the caller
    /// knows which deployment it actually called; the response only knows what answered.
    fn track_extracted(
        &self,
        provider: impl Into<ProviderId>,
        e: extract::Extracted,
        model: Option<&str>,
    ) {
        let m = model.map(str::to_string).or(e.model).unwrap_or_else(|| "unknown".into());
        self.event(provider, &m)
            .usage(e.input_tokens, e.output_tokens, e.cached_input_tokens)
            .send();
    }

    /// Drain and stop the background worker (call before exit). Dropping the client does the same.
    pub fn flush(self) {
        drop(self);
    }
}

/// Whether a body carries a usable project. The Rust client always *writes* `project_id` (an unset
/// project serializes as `""`), so presence is not enough — it has to be non-empty, which is exactly
/// the test the server's own ingest guard applies.
fn body_has_project(body: &Value) -> bool {
    body.get("project_id")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

/// Tag on the zero-usage event a locally-blocked call leaves behind.
pub const BLOCKED_TAG: &str = "lt_blocked_locally";

/// One send, decomposed off the `Response` so the same outcome can be both *read* (for the
/// admission cache) and *reported* (on stderr). A `Response` can only be consumed once, and the
/// limit signals were being thrown away with it.
enum SendOutcome {
    Answered {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    },
    Failed(reqwest::Error),
}

impl SendOutcome {
    fn of(outcome: reqwest::Result<reqwest::blocking::Response>) -> SendOutcome {
        match outcome {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let headers = resp
                    .headers()
                    .iter()
                    .filter_map(|(k, v)| {
                        v.to_str().ok().map(|s| (k.as_str().to_string(), s.to_string()))
                    })
                    .collect();
                let body = resp.text().unwrap_or_default();
                SendOutcome::Answered {
                    status,
                    headers,
                    body,
                }
            }
            Err(e) => SendOutcome::Failed(e),
        }
    }

    /// Fold this response into the admission cache. Never panics: a poisoned lock is recovered
    /// rather than propagated, because a panic on the worker thread would stop every later send.
    fn observe_into(&self, limits: &Mutex<AdmissionCache>) {
        let SendOutcome::Answered {
            status,
            headers,
            body,
        } = self
        else {
            return;
        };
        let parsed: Option<Value> = serde_json::from_str(body).ok();
        let view = limits::parse_limit_view(*status, headers, parsed.as_ref());
        let now = chrono::Utc::now().timestamp_millis();
        match limits.lock() {
            Ok(mut c) => c.observe(&view, now),
            Err(p) => p.into_inner().observe(&view, now),
        }
    }
}

/// Turn one send outcome into at most one stderr line. Runs on the worker thread, so it must never
/// panic: every failure path here is a `match`, not an `unwrap`.
fn report(
    diag: &Diagnostics,
    base_url: &str,
    path: &str,
    body: &Value,
    has_key: bool,
    outcome: SendOutcome,
) {
    let ctx = FailureContext {
        status: None,
        has_project: body_has_project(body),
        has_key,
    };
    match outcome {
        SendOutcome::Answered { status, .. } if (200..300).contains(&status) => {}
        SendOutcome::Answered { status, body, .. } => {
            // The server's explanation of a rejection is the whole point of the diagnostic.
            let detail = diagnostics::truncate(&body, 200);
            let ctx = FailureContext {
                status: Some(status),
                ..ctx
            };
            let msg = diagnostics::send_failure_message(
                base_url,
                path,
                &format!("HTTP {status} {detail}"),
                ctx,
            );
            diag.warn(&diagnostics::diagnostic_kind(Some(status), false), &msg);
        }
        SendOutcome::Failed(e) => {
            let kind = diagnostics::diagnostic_kind(None, e.is_timeout());
            let msg = diagnostics::send_failure_message(base_url, path, &e.to_string(), ctx);
            diag.warn(&kind, &msg);
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.tx.take(); // close the channel → worker drains queued events, then exits
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// Builder for one event; call [`EventBuilder::send`] to enqueue it.
pub struct EventBuilder<'a> {
    client: &'a Client,
    ev: LlmEvent,
}

impl<'a> EventBuilder<'a> {
    fn new(client: &'a Client, provider: ProviderId, model: String) -> Self {
        let ev = LlmEvent {
            id: lighttrack_core::new_id(),
            project_id: client.project.clone().unwrap_or_default(),
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            ts: chrono::Utc::now(),
            // Server-owned (`skip_deserializing` on the wire type): whatever we put here is ignored
            // and re-stamped on arrival. Filled only to satisfy the struct literal.
            received_at: chrono::Utc::now(),
            provider,
            model,
            name: None,
            operation: Operation::Chat,
            usage: TokenUsage::default(),
            cost_usd: None,
            latency_ms: None,
            status: Status::Success,
            error: None,
            input: None,
            output: None,
            tags: Vec::new(),
            source: client.source.clone(),
            metadata: Value::Null,
        };
        Self { client, ev }
    }

    pub fn project(mut self, p: impl Into<String>) -> Self {
        self.ev.project_id = p.into();
        self
    }
    pub fn input_tokens(mut self, n: u64) -> Self {
        self.ev.usage.input = n;
        self
    }
    pub fn output_tokens(mut self, n: u64) -> Self {
        self.ev.usage.output = n;
        self
    }
    pub fn cached_input(mut self, n: u64) -> Self {
        self.ev.usage.cached_input = Some(n);
        self
    }
    pub fn usage(mut self, input: u64, output: u64, cached: Option<u64>) -> Self {
        self.ev.usage.input = input;
        self.ev.usage.output = output;
        self.ev.usage.cached_input = cached;
        self
    }
    pub fn operation(mut self, op: Operation) -> Self {
        self.ev.operation = op;
        self
    }
    /// Set the use-case / call-site name (the LLM-Overview rollup key).
    pub fn name(mut self, n: impl Into<String>) -> Self {
        self.ev.name = Some(n.into());
        self
    }
    pub fn latency_ms(mut self, ms: u64) -> Self {
        self.ev.latency_ms = Some(ms);
        self
    }
    pub fn status(mut self, s: Status) -> Self {
        self.ev.status = s;
        self
    }
    pub fn error(mut self, e: impl Into<String>) -> Self {
        self.ev.error = Some(e.into());
        self.ev.status = Status::Error;
        self
    }
    pub fn input(mut self, v: Value) -> Self {
        self.ev.input = Some(v);
        self
    }
    pub fn output(mut self, v: Value) -> Self {
        self.ev.output = Some(v);
        self
    }
    pub fn tag(mut self, t: impl Into<String>) -> Self {
        self.ev.tags.push(t.into());
        self
    }
    pub fn trace_id(mut self, id: impl Into<String>) -> Self {
        self.ev.trace_id = Some(id.into());
        self
    }
    /// This span's own id — the handle a child call parents to. Without it (and `parent_span_id`)
    /// the server's `build_forest` nests strictly by `parent_span_id`→`span_id`, so every event is
    /// forced to a root and `GET /v1/traces/:id` renders flat, however many calls share a `trace_id`.
    /// The sibling Python/TS SDKs set these; this restores the trace-tree waterfall for Rust callers.
    pub fn span_id(mut self, id: impl Into<String>) -> Self {
        self.ev.span_id = Some(id.into());
        self
    }
    /// The enclosing span's id — set this to the parent call's `span_id` to nest under it.
    pub fn parent_span_id(mut self, id: impl Into<String>) -> Self {
        self.ev.parent_span_id = Some(id.into());
        self
    }
    pub fn metadata(mut self, v: Value) -> Self {
        self.ev.metadata = v;
        self
    }

    /// Finish the event without sending it — for callers who want to inspect or batch it and hand it
    /// to [`Client::track`] themselves. [`send`](Self::send) is `build` + `track`.
    pub fn build(self) -> LlmEvent {
        self.ev
    }

    /// Enqueue the event (best-effort, non-blocking).
    pub fn send(self) {
        self.client.track(self.ev);
    }
}

// ---- Output guardrails ------------------------------------------------------

/// Deterministic, network-free output validation rules. Build with `..Default::default()`:
/// `GuardRules { json: true, json_keys: vec!["id".into()], no_pii: true, ..Default::default() }`.
#[derive(Default, Clone)]
pub struct GuardRules {
    /// Output must parse as JSON.
    pub json: bool,
    /// Required top-level JSON keys (implies `json`).
    pub json_keys: Vec<String>,
    pub max_words: Option<usize>,
    pub min_words: Option<usize>,
    pub max_chars: Option<usize>,
    /// Substrings that must all appear.
    pub must_include: Vec<String>,
    /// Output must match this regex pattern.
    pub must_match: Option<String>,
    /// Regex patterns the output must NOT match (banned content / patterns).
    pub must_not_match: Vec<String>,
    /// Reject PII. The rules are the *server's* — generated from `lighttrack_anon`'s scrubber into
    /// `clients/contract/fixtures/pii.json` and embedded at compile time (see `pii.rs`), so this
    /// guard cannot contradict what the ingest path would redact. Kinds: `email`, `iban`, `ssn`,
    /// `secret`, `phone`, `credit_card`, `ip`.
    pub no_pii: bool,
}

/// Verdict from [`guard`]. `ok` is true iff `violations` is empty; `checks` lists each rule's result.
#[derive(Debug, Clone)]
pub struct GuardResult {
    pub ok: bool,
    pub violations: Vec<String>,
    pub checks: Vec<(String, bool)>,
}

/// Deterministic, network-free output validation — runs inline in the request path. Pure: returns a
/// verdict; the caller decides what to do (retry / fallback / block). Mirrors the TS/Python `guard`.
pub fn guard(output: &str, rules: &GuardRules) -> GuardResult {
    let mut violations: Vec<String> = Vec::new();
    let mut checks: Vec<(String, bool)> = Vec::new();
    let mut record = |key: String, passed: bool, msg: String| {
        checks.push((key, passed));
        if !passed {
            violations.push(msg);
        }
    };

    let want_json = rules.json || !rules.json_keys.is_empty();
    let mut parsed: Option<Value> = None;
    if want_json {
        match serde_json::from_str::<Value>(output.trim()) {
            Ok(v) => {
                parsed = Some(v);
                record("json".into(), true, String::new());
            }
            Err(_) => record("json".into(), false, "output is not valid JSON".into()),
        }
    }
    if let Some(obj) = parsed.as_ref().and_then(|v| v.as_object()) {
        for k in &rules.json_keys {
            record(
                format!("key:{k}"),
                obj.contains_key(k),
                format!("missing required JSON key '{k}'"),
            );
        }
    } else if !rules.json_keys.is_empty() && parsed.is_some() {
        // parsed but not an object: required keys cannot be satisfied
        for k in &rules.json_keys {
            record(
                format!("key:{k}"),
                false,
                format!("missing required JSON key '{k}'"),
            );
        }
    }

    let words = output.split_whitespace().count();
    if let Some(mw) = rules.max_words {
        record(
            "max_words".into(),
            words <= mw,
            format!("too long: {words} words > {mw}"),
        );
    }
    if let Some(mnw) = rules.min_words {
        record(
            "min_words".into(),
            words >= mnw,
            format!("too short: {words} words < {mnw}"),
        );
    }
    if let Some(mc) = rules.max_chars {
        let n = output.len();
        record(
            "max_chars".into(),
            n <= mc,
            format!("too long: {n} chars > {mc}"),
        );
    }
    for s in &rules.must_include {
        record(
            format!("include:{s}"),
            output.contains(s.as_str()),
            format!("must include \"{s}\""),
        );
    }
    if let Some(pat) = &rules.must_match {
        match regex::Regex::new(pat) {
            Ok(re) => record(
                "must_match".into(),
                re.is_match(output),
                format!("must match {pat}"),
            ),
            Err(_) => record(
                "must_match".into(),
                false,
                format!("invalid pattern: {pat}"),
            ),
        }
    }
    for pat in &rules.must_not_match {
        match regex::Regex::new(pat) {
            Ok(re) => record(
                format!("not_match:{pat}"),
                !re.is_match(output),
                format!("must not match {pat}"),
            ),
            Err(_) => record(
                format!("not_match:{pat}"),
                false,
                format!("invalid pattern: {pat}"),
            ),
        }
    }
    if rules.no_pii {
        let kinds = pii::pii_kinds(output);
        for kind in &kinds {
            record(format!("pii:{kind}"), false, format!("contains {kind}-like PII"));
        }
        if kinds.is_empty() {
            record("no_pii".into(), true, String::new());
        }
    }

    GuardResult {
        ok: violations.is_empty(),
        violations,
        checks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_catches_violations() {
        let r = guard(
            "{\"a\":1}",
            &GuardRules {
                json_keys: vec!["a".into(), "b".into()],
                ..Default::default()
            },
        );
        assert!(!r.ok);
        assert!(r.violations.iter().any(|v| v.contains("'b'")));

        let r = guard(
            "one two three four five",
            &GuardRules {
                max_words: Some(3),
                ..Default::default()
            },
        );
        assert!(!r.ok);

        let r = guard(
            "reach me at alice@example.com",
            &GuardRules {
                no_pii: true,
                ..Default::default()
            },
        );
        assert!(!r.ok);
        assert!(r.violations.iter().any(|v| v.contains("email")));
    }

    #[test]
    fn guard_passes_valid() {
        let r = guard(
            "{\"merchant\":\"X\",\"total\":1.5}",
            &GuardRules {
                json_keys: vec!["merchant".into(), "total".into()],
                max_chars: Some(200),
                no_pii: true,
                ..Default::default()
            },
        );
        assert!(r.ok, "violations: {:?}", r.violations);
    }

    #[test]
    fn span_setters_build_a_nestable_tree() {
        // No network: an unconfigured client just drops events; we only build() them here.
        let c = Client::new("http://127.0.0.1:0", None, Some("p1".into()));

        // A planner span and a tool call parented to it — the structure a Rust agent instruments for.
        let plan = c
            .event("anthropic", "claude-sonnet-4-5")
            .trace_id("req-123")
            .span_id("s-plan")
            .name("plan")
            .usage(10, 5, None)
            .build();
        let tool = c
            .event("anthropic", "claude-sonnet-4-5")
            .trace_id("req-123")
            .span_id("s-tool")
            .parent_span_id("s-plan")
            .name("tool")
            .usage(10, 5, None)
            .build();

        assert_eq!(plan.span_id.as_deref(), Some("s-plan"));
        assert_eq!(tool.parent_span_id.as_deref(), Some("s-plan"));

        // Fed through the server's own forest builder, the tool nests UNDER the planner instead of
        // both being sibling roots (the flat rendering the missing setters used to force).
        let trace = lighttrack_core::Trace::from_events(vec![plan, tool]).expect("a trace");
        assert_eq!(
            trace.spans.len(),
            1,
            "one root, not two: {:?}",
            trace.spans.len()
        );
        assert_eq!(trace.spans[0].event.span_id.as_deref(), Some("s-plan"));
        assert_eq!(
            trace.spans[0].children.len(),
            1,
            "tool nested under planner"
        );
        assert_eq!(
            trace.spans[0].children[0].event.span_id.as_deref(),
            Some("s-tool")
        );
    }
}
