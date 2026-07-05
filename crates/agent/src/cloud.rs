//! Thin HTTP client for one cloud LightTrack source (lease + settle), bearer-authenticated with
//! that source's device key.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use lighttrack_core::RelayTask;

use crate::config::Source;
use crate::exec::RunReport;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct Client {
    pub name: String,
    base: String,
    key: String,
    http: reqwest::blocking::Client,
}

impl Client {
    pub(crate) fn new(source: &Source) -> Result<Self> {
        Ok(Self {
            name: source.name.clone(),
            base: source.url.clone(),
            key: source.key()?,
            http: reqwest::blocking::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()
                .context("building cloud HTTP client")?,
        })
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.key)
            .json(body)
            .send()
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            bail!("POST {path} -> HTTP {}: {text}", status.as_u16());
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Lease up to `max` due tasks for `device`, held for `lease_secs`; the server long-polls up
    /// to `wait_secs` before answering empty.
    pub(crate) fn lease(
        &self,
        device: &str,
        max: usize,
        lease_secs: i64,
        wait_secs: u64,
    ) -> Result<Vec<RelayTask>> {
        let v = self.post(
            "/v1/relay/lease",
            &json!({ "device": device, "max": max, "lease_secs": lease_secs, "wait_secs": wait_secs }),
        )?;
        serde_json::from_value(v).context("decoding leased tasks")
    }

    /// Settle one task with the run's outcome + usage accounting.
    pub(crate) fn settle(&self, task_id: &str, report: &RunReport) -> Result<()> {
        self.post(
            &format!("/v1/relay/tasks/{task_id}/result"),
            &json!({
                "status": report.status,
                "result": report.result,
                "error": report.error,
                "retry_after_secs": report.retry_after_secs,
                "model": report.model,
                "input_tokens": report.input_tokens,
                "output_tokens": report.output_tokens,
                "latency_ms": report.latency_ms,
            }),
        )?;
        Ok(())
    }
}
