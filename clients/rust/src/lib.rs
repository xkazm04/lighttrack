//! LightTrack Rust client — fire-and-forget LLM event ingestion.
//!
//! Reuses [`lighttrack_core::LlmEvent`] as the wire type, so the payload can never drift from the
//! API. Sends are best-effort and non-blocking: events go to a background worker thread over a
//! channel, which POSTs them. The worker drains and joins when the [`Client`] is dropped (or on an
//! explicit [`Client::flush`]).
//!
//! ```no_run
//! use lighttrack_client::{Client, Provider};
//! let lt = Client::from_env();
//! lt.event(Provider::OpenAi, "gpt-4o")
//!     .input_tokens(120).output_tokens(45).latency_ms(210)
//!     .send();
//! lt.flush(); // drain the background worker before exit
//! ```

use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;

pub use lighttrack_core::{Operation, Provider, Status};
use lighttrack_core::{LlmEvent, TokenUsage};

const DEFAULT_URL: &str = "http://127.0.0.1:8787";

/// A best-effort, non-blocking ingestion client. Cheap to construct; events are POSTed from a
/// background thread. Configure via [`Client::from_env`] or [`Client::new`].
pub struct Client {
    project: Option<String>,
    source: Option<String>,
    tx: Option<Sender<(&'static str, Value)>>,
    worker: Option<JoinHandle<()>>,
}

impl Client {
    /// Build from `LIGHTTRACK_URL`, `LIGHTTRACK_KEY`, `LIGHTTRACK_PROJECT`.
    pub fn from_env() -> Self {
        Self::new(
            std::env::var("LIGHTTRACK_URL").unwrap_or_else(|_| DEFAULT_URL.to_string()),
            std::env::var("LIGHTTRACK_KEY").ok().filter(|s| !s.is_empty()),
            std::env::var("LIGHTTRACK_PROJECT").ok().filter(|s| !s.is_empty()),
        )
    }

    /// A project key derives the project server-side; set `project` only for dev mode (no key) or an
    /// admin key ingesting into a specific project.
    pub fn new(base_url: impl Into<String>, api_key: Option<String>, project: Option<String>) -> Self {
        let base = base_url.into().trim_end_matches('/').to_string();
        let (tx, rx) = mpsc::channel::<(&'static str, Value)>();
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
                    let mut req = http.post(format!("{base}{path}")).json(&body);
                    if let Some(k) = &api_key {
                        req = req.bearer_auth(k);
                    }
                    let _ = req.send(); // best-effort: telemetry must never break the host app
                }
            })
            .ok();
        Self { project, source: None, tx: Some(tx), worker }
    }

    /// Set a `source` label stamped on every event.
    pub fn source(mut self, s: impl Into<String>) -> Self {
        self.source = Some(s.into());
        self
    }

    /// Start building an event for one LLM call.
    pub fn event(&self, provider: Provider, model: impl Into<String>) -> EventBuilder<'_> {
        EventBuilder::new(self, provider, model.into())
    }

    /// Low-level: enqueue a fully-built event (best-effort, non-blocking).
    pub fn track(&self, ev: LlmEvent) {
        self.send_raw("/v1/events", serde_json::to_value(&ev).unwrap_or(Value::Null));
    }

    /// Enqueue a pre-serialized body to an API path (best-effort, non-blocking).
    fn send_raw(&self, path: &'static str, body: Value) {
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
            rubric: name.map(|n| format!("guard:{n}")).unwrap_or_else(|| "guard".into()),
            value: if result.ok { 1.0 } else { 0.0 },
            max: 1.0,
            pass: Some(result.ok),
            reasoning: Some(if result.violations.is_empty() {
                "all checks passed".to_string()
            } else {
                result.violations.join("; ")
            }),
            scored_by: self
                .source
                .clone()
                .map(|s| format!("guard:{s}"))
                .unwrap_or_else(|| "lighttrack-guard".into()),
            cost_usd: None,
            created_at: chrono::Utc::now(),
        };
        self.send_raw("/v1/scores", serde_json::to_value(&score).unwrap_or(Value::Null));
        result
    }

    /// Track from an OpenAI chat/responses JSON value (extracts model + token usage).
    pub fn track_openai_json(&self, resp: &Value, model: Option<&str>) {
        let u = &resp["usage"];
        let input = u["prompt_tokens"].as_u64().or_else(|| u["input_tokens"].as_u64()).unwrap_or(0);
        let output = u["completion_tokens"].as_u64().or_else(|| u["output_tokens"].as_u64()).unwrap_or(0);
        let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64();
        let m = model.or_else(|| resp["model"].as_str()).unwrap_or("unknown");
        self.event(Provider::OpenAi, m).usage(input, output, cached).send();
    }

    /// Track from an Anthropic messages JSON value.
    pub fn track_anthropic_json(&self, resp: &Value, model: Option<&str>) {
        let u = &resp["usage"];
        let input = u["input_tokens"].as_u64().unwrap_or(0);
        let output = u["output_tokens"].as_u64().unwrap_or(0);
        let cached = u["cache_read_input_tokens"].as_u64();
        let m = model.or_else(|| resp["model"].as_str()).unwrap_or("unknown");
        self.event(Provider::Anthropic, m).usage(input, output, cached).send();
    }

    /// Track from a Gemini generateContent JSON value (model is usually passed in).
    pub fn track_gemini_json(&self, resp: &Value, model: Option<&str>) {
        let u = &resp["usageMetadata"];
        let input = u["promptTokenCount"].as_u64().unwrap_or(0);
        let output = u["candidatesTokenCount"].as_u64().unwrap_or(0);
        let cached = u["cachedContentTokenCount"].as_u64();
        let m = model.or_else(|| resp["modelVersion"].as_str()).unwrap_or("unknown");
        self.event(Provider::Google, m).usage(input, output, cached).send();
    }

    /// Drain and stop the background worker (call before exit). Dropping the client does the same.
    pub fn flush(self) {
        drop(self);
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
    fn new(client: &'a Client, provider: Provider, model: String) -> Self {
        let ev = LlmEvent {
            id: lighttrack_core::new_id(),
            project_id: client.project.clone().unwrap_or_default(),
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            ts: chrono::Utc::now(),
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
    pub fn metadata(mut self, v: Value) -> Self {
        self.ev.metadata = v;
        self
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
    /// Reject common PII (email, phone, credit-card-like, SSN).
    pub no_pii: bool,
}

/// Verdict from [`guard`]. `ok` is true iff `violations` is empty; `checks` lists each rule's result.
#[derive(Debug, Clone)]
pub struct GuardResult {
    pub ok: bool,
    pub violations: Vec<String>,
    pub checks: Vec<(String, bool)>,
}

const PII_PATTERNS: [(&str, &str); 4] = [
    ("email", r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}"),
    ("phone", r"(?:\+?\d[\s().-]?){10,}"),
    ("credit_card", r"\b(?:\d[ -]?){13,16}\b"),
    ("ssn", r"\b\d{3}-\d{2}-\d{4}\b"),
];

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
            record(format!("key:{k}"), obj.contains_key(k), format!("missing required JSON key '{k}'"));
        }
    } else if !rules.json_keys.is_empty() && parsed.is_some() {
        // parsed but not an object: required keys cannot be satisfied
        for k in &rules.json_keys {
            record(format!("key:{k}"), false, format!("missing required JSON key '{k}'"));
        }
    }

    let words = output.split_whitespace().count();
    if let Some(mw) = rules.max_words {
        record("max_words".into(), words <= mw, format!("too long: {words} words > {mw}"));
    }
    if let Some(mnw) = rules.min_words {
        record("min_words".into(), words >= mnw, format!("too short: {words} words < {mnw}"));
    }
    if let Some(mc) = rules.max_chars {
        let n = output.len();
        record("max_chars".into(), n <= mc, format!("too long: {n} chars > {mc}"));
    }
    for s in &rules.must_include {
        record(format!("include:{s}"), output.contains(s.as_str()), format!("must include \"{s}\""));
    }
    if let Some(pat) = &rules.must_match {
        match regex::Regex::new(pat) {
            Ok(re) => record("must_match".into(), re.is_match(output), format!("must match {pat}")),
            Err(_) => record("must_match".into(), false, format!("invalid pattern: {pat}")),
        }
    }
    for pat in &rules.must_not_match {
        match regex::Regex::new(pat) {
            Ok(re) => record(format!("not_match:{pat}"), !re.is_match(output), format!("must not match {pat}")),
            Err(_) => record(format!("not_match:{pat}"), false, format!("invalid pattern: {pat}")),
        }
    }
    if rules.no_pii {
        let mut clean = true;
        for (name, pat) in PII_PATTERNS {
            if let Ok(re) = regex::Regex::new(pat) {
                if re.is_match(output) {
                    clean = false;
                    record(format!("pii:{name}"), false, format!("contains {name}-like PII"));
                }
            }
        }
        if clean {
            record("no_pii".into(), true, String::new());
        }
    }

    GuardResult { ok: violations.is_empty(), violations, checks }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_catches_violations() {
        let r = guard("{\"a\":1}", &GuardRules { json_keys: vec!["a".into(), "b".into()], ..Default::default() });
        assert!(!r.ok);
        assert!(r.violations.iter().any(|v| v.contains("'b'")));

        let r = guard("one two three four five", &GuardRules { max_words: Some(3), ..Default::default() });
        assert!(!r.ok);

        let r = guard("reach me at alice@example.com", &GuardRules { no_pii: true, ..Default::default() });
        assert!(!r.ok);
        assert!(r.violations.iter().any(|v| v.contains("email")));
    }

    #[test]
    fn guard_passes_valid() {
        let r = guard(
            "{\"merchant\":\"X\",\"total\":1.5}",
            &GuardRules { json_keys: vec!["merchant".into(), "total".into()], max_chars: Some(200), no_pii: true, ..Default::default() },
        );
        assert!(r.ok, "violations: {:?}", r.violations);
    }
}
