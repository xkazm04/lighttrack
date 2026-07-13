//! Thin blocking HTTP client for the LightTrack API.
//!
//! The MCP server never touches the database directly — it only makes HTTP calls that the API
//! validates. That's the safety boundary: a misbehaving tool call can at worst get a 4xx; it cannot
//! corrupt state or crash the API process.

use serde_json::Value;

pub(crate) struct Client {
    base: String,
    key: Option<String>,
    http: reqwest::blocking::Client,
}

impl Client {
    pub(crate) fn from_env() -> Self {
        Self {
            base: std::env::var("LIGHTTRACK_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8787".into()),
            key: std::env::var("LIGHTTRACK_KEY").ok().filter(|s| !s.is_empty()),
            http: reqwest::blocking::Client::new(),
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
