//! What the gateway tells LightTrack about each call — every attempt, honestly.
//!
//! This is the bug the app-side wrappers kept shipping, fixed once: a failed primary is an
//! **error** event on the model that failed, and a fallback success is a success on the model
//! that answered, tagged `fell_back`. All attempts of one request share a `trace_id`, so the
//! trace view shows the failover as one story rather than two unrelated rows.
//!
//! Emission is fire-and-forget on the runtime: a slow or absent API never delays the answer.

use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Value};

use lighttrack_core::{new_id, split_effort, FAILURE_CLASS_KEY};

use crate::failover::{AttemptKind, ChainResult};
use crate::wire::Prepared;

/// Where events go. `key` is a project key (`Authorization: Bearer`); without one, `project`
/// scopes the write in a dev-mode API.
#[derive(Debug, Clone)]
pub struct Telemetry {
    pub client: reqwest::Client,
    pub url: String,
    pub key: Option<String>,
    pub project: Option<String>,
    /// Whether `input`/`output` ride on the event. The project's redaction policy still applies
    /// on the API side; this is the gateway's own switch for never sending them at all.
    pub record_content: bool,
}

impl Telemetry {
    /// `None` when neither a key nor a project is configured — nothing to attribute events to.
    pub fn from_env() -> Option<Telemetry> {
        let url =
            std::env::var("LIGHTTRACK_URL").unwrap_or_else(|_| "http://127.0.0.1:8787".into());
        let key = std::env::var("LIGHTTRACK_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        let project = std::env::var("LIGHTTRACK_PROJECT")
            .ok()
            .filter(|s| !s.is_empty());
        if key.is_none() && project.is_none() {
            return None;
        }
        let record_content = !matches!(
            std::env::var("LIGHTTRACK_GATEWAY_OMIT_CONTENT").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        );
        Some(Telemetry {
            client: reqwest::Client::new(),
            url: url.trim_end_matches('/').to_string(),
            key,
            project,
            record_content,
        })
    }

    pub fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self.client.request(method, format!("{}{}", self.url, path));
        if let Some(k) = &self.key {
            req = req.bearer_auth(k);
        }
        if let Some(p) = &self.project {
            req = req.query(&[("project", p)]);
        }
        req
    }

    /// Post the request's events in the background. Failures are logged, never raised.
    pub fn emit(self: &Arc<Self>, events: Vec<Value>) {
        if events.is_empty() {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let resp = me
                .request(reqwest::Method::POST, "/v1/events/batch")
                .json(&events)
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => eprintln!(
                    "[gateway] telemetry: API answered {} for {} event(s)",
                    r.status(),
                    events.len()
                ),
                Err(e) => eprintln!("[gateway] telemetry: could not reach the API: {e}"),
            }
        });
    }
}

/// One event per attempt that actually called a seat. A skipped (cooling) target made no call
/// and costs no row; it is recorded on the served event's metadata instead.
pub fn events_for(
    tel: &Telemetry,
    request_id: &str,
    route: Option<&str>,
    name: Option<&str>,
    prepared: &Prepared,
    chain: &ChainResult,
) -> Vec<Value> {
    let prompt = &prepared.req;
    let ts = Utc::now();
    let skipped: Vec<String> = chain
        .attempts
        .iter()
        .filter(|a| matches!(a.kind, AttemptKind::SkippedCooling { .. }))
        .map(|a| a.target.spec())
        .collect();
    let mut out = Vec::new();
    for (i, a) in chain.attempts.iter().enumerate() {
        let (status, error, usage, cost, model, output, tags, verdict) = match &a.kind {
            AttemptKind::SkippedCooling { .. } => continue,
            AttemptKind::Served(o) => (
                "success",
                None,
                json!({
                    "input": o.gen.input_tokens.unwrap_or(0),
                    "output": o.gen.output_tokens.unwrap_or(0),
                    "reasoning": o.gen.reasoning_tokens,
                }),
                o.gen.cost_usd,
                o.gen.model.clone(),
                Some(match &o.tool_calls {
                    Some(calls) => json!({ "content": o.gen.output, "tool_calls": calls }),
                    None => Value::String(o.gen.output.clone()),
                }),
                if i > 0 {
                    vec!["gateway", "fell_back"]
                } else {
                    vec!["gateway"]
                },
                None,
            ),
            AttemptKind::Failed { error, verdict } => (
                "error",
                Some(error.clone()),
                json!({ "input": 0, "output": 0 }),
                None,
                split_effort(&a.target.model).0.to_string(),
                None,
                vec!["gateway", "provider_failed"],
                Some(verdict.as_str()),
            ),
        };
        let mut metadata = json!({
            "gateway": {
                "route": route,
                "attempt": i + 1,
                "chain_len": chain.attempts.len(),
                "target": a.target.spec(),
                "verdict": verdict,
                "skipped_cooling": skipped,
                "transcript": prepared.transcript,
                "tools": prompt.has_tools(),
            }
        });
        if status == "error" {
            // Every failure the chain moves past is one another seat could answer — transient by
            // construction. A terminal one stopped the chain and is the request's own fault.
            metadata[FAILURE_CLASS_KEY] = Value::String(
                if verdict == Some("terminal") {
                    "terminal"
                } else {
                    "transient"
                }
                .into(),
            );
        }
        let mut ev = json!({
            "id": new_id(),
            "trace_id": request_id,
            "span_id": new_id(),
            "ts": ts.to_rfc3339(),
            "provider": a.target.provider,
            "model": model,
            "name": name,
            "operation": "chat",
            "usage": usage,
            "cost_usd": cost,
            "latency_ms": a.latency_ms,
            "status": status,
            "error": error,
            "tags": tags,
            "source": "lt-gateway",
            "metadata": metadata,
        });
        // A keyed API derives the project from the key; a dev-mode API reads it from the body
        // (the query string scopes reads, not writes), so stamp it whenever we know it.
        if let Some(p) = &tel.project {
            ev["project_id"] = Value::String(p.clone());
        }
        if tel.record_content {
            // The input as the engine saw it: the system prompt and every turn, so a rendered
            // conversation and a native one read the same in the store.
            ev["input"] = json!({ "system": prompt.system, "messages": prompt.openai_messages()
                .into_iter().filter(|m| m["role"] != "system").collect::<Vec<_>>() });
            if let Some(o) = output {
                ev["output"] = o;
            }
        }
        out.push(ev);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failover::{Attempt, Verdict};
    use crate::target::Target;
    use lighttrack_engine::{ChatOutcome, ChatRequest, Determinism, GenOutcome, SchemaEnforcement};

    fn tel(record: bool) -> Telemetry {
        Telemetry {
            client: reqwest::Client::new(),
            url: "http://x".into(),
            key: None,
            project: Some("p".into()),
            record_content: record,
        }
    }

    fn chain() -> ChainResult {
        ChainResult {
            attempts: vec![
                Attempt {
                    target: Target::parse("anthropic/claude-sonnet-5@medium").unwrap(),
                    latency_ms: 20,
                    kind: AttemptKind::Failed {
                        error: "usage limit".into(),
                        verdict: Verdict::Exhausted(std::time::Duration::from_secs(1)),
                    },
                },
                Attempt {
                    target: Target::parse("codex/gpt-5.5").unwrap(),
                    latency_ms: 900,
                    kind: AttemptKind::Served(ChatOutcome::text(GenOutcome {
                        output: "answer".into(),
                        cost_usd: None,
                        model: "gpt-5.5".into(),
                        latency_ms: Some(900),
                        input_tokens: Some(100),
                        output_tokens: Some(20),
                        reasoning_tokens: Some(5),
                        determinism: Determinism::BestEffort,
                        schema: SchemaEnforcement::NotRequested,
                    })),
                },
            ],
            served: Some(1),
        }
    }

    #[test]
    fn a_failed_primary_is_an_error_row_and_the_fallback_a_tagged_success_on_one_trace() {
        let p = Prepared {
            req: ChatRequest::single(Some("s"), "i", None),
            transcript: false,
        };
        let evs = events_for(
            &tel(true),
            "req-1",
            Some("summarize"),
            Some("summarize"),
            &p,
            &chain(),
        );
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0]["status"], "error");
        assert_eq!(evs[0]["model"], "claude-sonnet-5");
        assert_eq!(evs[0]["metadata"]["failure_class"], "transient");
        assert!(evs[0]["tags"]
            .as_array()
            .unwrap()
            .contains(&json!("provider_failed")));
        assert_eq!(evs[1]["status"], "success");
        assert_eq!(evs[1]["provider"], "codex");
        assert!(evs[1]["tags"]
            .as_array()
            .unwrap()
            .contains(&json!("fell_back")));
        assert_eq!(evs[1]["usage"]["reasoning"], 5);
        assert_eq!(evs[0]["trace_id"], evs[1]["trace_id"]);
        assert_eq!(evs[0]["project_id"], "p");
        assert_eq!(evs[1]["output"], "answer");
    }

    #[test]
    fn content_stays_home_when_asked() {
        let p = Prepared {
            req: ChatRequest::single(None, "secret", None),
            transcript: false,
        };
        let evs = events_for(&tel(false), "r", None, None, &p, &chain());
        assert!(evs[1].get("output").is_none());
        assert!(evs[1].get("input").is_none());
    }
}
