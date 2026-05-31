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
    tx: Option<Sender<LlmEvent>>,
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
        let (tx, rx) = mpsc::channel::<LlmEvent>();
        let worker = std::thread::Builder::new()
            .name("lighttrack".into())
            .spawn(move || {
                let http = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(2))
                    .build()
                    .unwrap_or_else(|_| reqwest::blocking::Client::new());
                // Receives until all senders drop; delivers queued events first, so Drop drains.
                while let Ok(ev) = rx.recv() {
                    let mut req = http.post(format!("{base}/v1/events")).json(&ev);
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
        if let Some(tx) = &self.tx {
            let _ = tx.send(ev);
        }
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
