//! Thin blocking HTTP client for the LightTrack API.
//!
//! The MCP server never touches the database directly — it only makes HTTP calls that the API
//! validates. That's the safety boundary: a misbehaving tool call can at worst get a 4xx; it cannot
//! corrupt state or crash the API process.

use std::time::Duration;

use serde_json::Value;

/// How long a single API call may take before it becomes an in-band error.
///
/// This transport is **stdio**: one process, one session, one request at a time. A call with no
/// timeout does not degrade — it hangs the only session the design has, and the agent on the other
/// end has no channel to ask what happened. `reqwest`'s default is no timeout at all, so an API that
/// accepted the connection and then stopped answering (a paused container, a dropped route, a
/// deadlocked handler) parked `lt-mcp` forever.
///
/// 30 s is chosen against the slowest read this server makes (a large filtered event page over a
/// cold Postgres), with room to spare: long enough that a legitimate call is never cut off, short
/// enough that a wedged upstream costs half a minute instead of the session.
/// `LIGHTTRACK_MCP_TIMEOUT_SECS` overrides it; `0` disables it, for someone deliberately debugging a
/// slow upstream who would rather wait than retry.
const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub(crate) const ENV_TIMEOUT: &str = "LIGHTTRACK_MCP_TIMEOUT_SECS";

pub(crate) struct Client {
    base: String,
    key: Option<String>,
    http: reqwest::blocking::Client,
}

impl Client {
    pub(crate) fn from_env() -> Self {
        let timeout = std::env::var(ENV_TIMEOUT)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let mut builder = reqwest::blocking::Client::builder();
        if timeout > 0 {
            builder = builder.timeout(Duration::from_secs(timeout));
        }
        Self {
            // Trimmed once: `LIGHTTRACK_URL=https://host/` is how a URL gets pasted, and the naive
            // join produced `https://host//v1/...` - a 404 on every tool that read as "no such
            // endpoint" (the CLI had the identical defect).
            base: std::env::var("LIGHTTRACK_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8787".into())
                .trim_end_matches('/')
                .to_string(),
            key: std::env::var("LIGHTTRACK_KEY")
                .ok()
                .filter(|s| !s.is_empty()),
            // A builder failure here is a TLS/backend initialization problem, not a per-call one;
            // falling back to an untimed client would silently restore the hang this exists to
            // prevent, so the process refuses to start instead.
            http: builder
                .build()
                .expect("HTTP client init (TLS backend unavailable)"),
        }
    }

    pub(crate) fn base(&self) -> &str {
        &self.base
    }

    pub(crate) fn get(&self, path: &str) -> Result<Value, String> {
        self.send(self.http.get(self.url(path)))
    }

    /// Like [`get`], but also returns the `X-Next-Cursor` header (the keyset cursor for the next page)
    /// when the API sets it. Used by the paged list tools so an agent can walk past the first page.
    pub(crate) fn get_paged(&self, path: &str) -> Result<(Value, Option<String>), String> {
        self.send_full(self.http.get(self.url(path)))
    }

    pub(crate) fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.send(self.http.post(self.url(path)).json(body))
    }

    pub(crate) fn put(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.send(self.http.put(self.url(path)).json(body))
    }

    pub(crate) fn delete(&self, path: &str) -> Result<Value, String> {
        self.send(self.http.delete(self.url(path)))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn send(&self, req: reqwest::blocking::RequestBuilder) -> Result<Value, String> {
        self.send_full(req).map(|(v, _)| v)
    }

    /// The shared request path: attach auth, send, and return the parsed body plus the
    /// `X-Next-Cursor` header (headers of interest). On a non-2xx it preserves the API's status and
    /// body as `HTTP {code}: {body}` so callers can map it to actionable guidance.
    fn send_full(
        &self,
        mut req: reqwest::blocking::RequestBuilder,
    ) -> Result<(Value, Option<String>), String> {
        if let Some(k) = &self.key {
            req = req.bearer_auth(k);
        }
        let resp = req.send().map_err(|e| e.to_string())?;
        let status = resp.status();
        // Grab the header before `text()` consumes the response.
        let next_cursor = resp
            .headers()
            .get("x-next-cursor")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let text = resp.text().map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("HTTP {}: {text}", status.as_u16()));
        }
        if text.trim().is_empty() {
            return Ok((Value::Null, next_cursor));
        }
        let value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        Ok((value, next_cursor))
    }
}
