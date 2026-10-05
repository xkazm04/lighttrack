//! Thin HTTP client for one cloud LightTrack source (lease + settle), bearer-authenticated with
//! that source's device key.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use lighttrack_core::RelayTask;

use crate::config::Source;
use crate::report::RunReport;
use crate::verdict::{self, Verdict};

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

    /// One HTTP answer, uninterpreted. An `Err` here is a transport failure and nothing else: a
    /// refusal is a status code, and folding it into an error string is how the status got lost and
    /// had to be guessed back out of prose (see `crate::verdict`).
    fn post_raw(&self, path: &str, body: &Value) -> Result<Answer> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.key)
            .json(body)
            .send()
            .with_context(|| format!("POST {url}"))?;
        Ok(Answer {
            status: resp.status().as_u16(),
            body: resp.text().unwrap_or_default(),
        })
    }

    /// For the doors whose only two outcomes are "it worked" and "give up this round" — the lease
    /// itself. Every door that carries an ownership decision goes through `post_raw`.
    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let a = self.post_raw(path, body)?;
        if !(200..300).contains(&a.status) {
            bail!("POST {path} -> HTTP {}: {}", a.status, a.body);
        }
        Ok(serde_json::from_str(&a.body).unwrap_or(Value::Null))
    }

    /// Lease up to `max` due tasks this device can run, held for `lease_secs`; the server
    /// long-polls up to `wait_secs` before answering empty.
    ///
    /// `capabilities` is the device's own action inventory (M18) — the cloud filters the lease
    /// against it, so an action never reaches a device whose library lacks it and burns an attempt
    /// discovering that. There is no `device` argument any more: identity comes from the key, and
    /// the name this agent used to assert was something the cloud wrote down as fact.
    pub(crate) fn lease(
        &self,
        capabilities: &[String],
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
            &json!({
                "capabilities": capabilities,
                "agent_version": env!("CARGO_PKG_VERSION"),
                "max": max,
                "lease_secs": lease_secs,
                "wait_secs": wait_secs,
            }),
        )?;
        let r: Resp = serde_json::from_value(v).context("decoding leased tasks")?;
        Ok(Lease {
            tasks: r.tasks,
            renew_secs: r.renew_secs.max(1),
        })
    }

    /// Prove this device is still running `task_id`, and read the answer. The renewal is the only
    /// channel that reaches a live device, so it carries three different facts — still yours, stop,
    /// not yours any more — which `crate::verdict` tells apart from the status and the typed body.
    /// A blip must not abandon a healthy run; a takeover must stop one; a cancel must stop one
    /// without abandoning its report.
    pub(crate) fn renew(&self, task_id: &str, fence: DateTime<Utc>) -> Verdict {
        verdict::classify(self.post_raw(
            &format!("/v1/relay/tasks/{task_id}/renew"),
            &json!({ "fence": fence }),
        ))
    }

    /// Settle one task with the run's outcome + usage accounting, carrying the lease fence. A 409
    /// here means the report was NOT recorded — this device no longer owns the task.
    ///
    /// Classified through the same authority as the renewal: one taxonomy, used by every door that
    /// decides ownership. The alternative is what was here — a second substring test, in a second
    /// module, free to drift from the first.
    pub(crate) fn settle(
        &self,
        task_id: &str,
        fence: Option<DateTime<Utc>>,
        report: &RunReport,
    ) -> Verdict {
        verdict::classify(self.post_raw(
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
                // Which prompt ran (always) and what it said (only under the action's `report_io`).
                // `input`/`output` are `null` for an action that has not opted in, which is what
                // makes the settle event unjudgeable-by-default rather than unjudgeable-by-design.
                "prompt_sha256": report.prompt_sha256,
                "action_version": report.action_version,
                "input": report.rendered_prompt,
                "output": report.result_text,
                "fence": fence,
            }),
        ))
    }
}

/// One HTTP answer before anything interprets it: the status code and the body text, both intact.
pub(crate) struct Answer {
    pub status: u16,
    pub body: String,
}

/// What a lease hands back: the tasks, and the renewal contract that comes with them. The cadence
/// is the SERVER's — it clamps the requested TTL — so the agent reads it rather than deriving one
/// from a number the server may not have honoured.
pub(crate) struct Lease {
    pub tasks: Vec<RelayTask>,
    pub renew_secs: u64,
}
