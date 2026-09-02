//! Thin HTTP client for one cloud LightTrack source (lease + settle), bearer-authenticated with
//! that source's device key.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
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
    ) -> Result<Lease> {
        #[derive(Deserialize)]
        struct Resp {
            tasks: Vec<RelayTask>,
            #[serde(default = "one")]
            renew_secs: u64,
        }
        fn one() -> u64 {
            1
        }
        let v = self.post(
            "/v1/relay/lease",
            &json!({ "device": device, "max": max, "lease_secs": lease_secs, "wait_secs": wait_secs }),
        )?;
        let r: Resp = serde_json::from_value(v).context("decoding leased tasks")?;
        Ok(Lease {
            tasks: r.tasks,
            renew_secs: r.renew_secs.max(1),
        })
    }

    /// Prove this device is still running `task_id`. `Ok(false)` means the lease is no longer ours
    /// (HTTP 409) — affirmative evidence of a takeover, and a DIFFERENT answer from `Err`, which
    /// only means "I could not tell". A blip must not abandon a healthy run; a takeover must stop
    /// one.
    pub(crate) fn renew(&self, task_id: &str, fence: DateTime<Utc>) -> Result<bool> {
        match self.post(
            &format!("/v1/relay/tasks/{task_id}/renew"),
            &json!({ "fence": fence }),
        ) {
            Ok(_) => Ok(true),
            Err(e) if is_conflict(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Settle one task with the run's outcome + usage accounting, carrying the lease fence. A 409
    /// here means the report was NOT recorded — this device no longer owns the task.
    pub(crate) fn settle(
        &self,
        task_id: &str,
        fence: Option<DateTime<Utc>>,
        report: &RunReport,
    ) -> Result<()> {
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
                "cost_usd": report.cost_usd,
                "mode": report.mode,
                "fence": fence,
            }),
        )?;
        Ok(())
    }
}

/// What a lease hands back: the tasks, and the renewal contract that comes with them. The cadence
/// is the SERVER's — it clamps the requested TTL — so the agent reads it rather than deriving one
/// from a number the server may not have honoured.
pub(crate) struct Lease {
    pub tasks: Vec<RelayTask>,
    pub renew_secs: u64,
}

/// Whether an error is the 409 that means "you do not hold this any more".
pub(crate) fn is_conflict(e: &anyhow::Error) -> bool {
    e.to_string().contains("HTTP 409")
}
