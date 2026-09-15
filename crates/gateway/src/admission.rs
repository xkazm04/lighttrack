//! Pre-spend admission: ask the API whether the project's limits are enforcing *before* a seat
//! is spent. This is the inline block the architecture deferred to "gateway mode" — the ingest
//! door can only refuse to *record* a call that already cost something; this door refuses the
//! call.
//!
//! Fail-open by design: an API that cannot be reached leaves the call admitted and says so on
//! stderr. The gateway must never turn an observability outage into an app outage.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::telemetry::Telemetry;

/// How long one verdict is reused. Limits move on the scale of minutes; a poll per call would
/// put the API on every request's critical path for nothing.
const TTL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct Verdict {
    pub throttled: bool,
    /// The binding rule(s), as the API described them.
    pub detail: Value,
}

pub struct Admission {
    cache: Mutex<Option<(Instant, Verdict)>>,
}

impl Default for Admission {
    fn default() -> Self {
        Admission {
            cache: Mutex::new(None),
        }
    }
}

impl Admission {
    /// The current verdict, from cache or a fresh poll.
    pub async fn check(&self, tel: &Telemetry) -> Verdict {
        {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((at, v)) = cache.as_ref() {
                if at.elapsed() < TTL {
                    return v.clone();
                }
            }
        }
        let verdict = match poll(tel).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[gateway] admission: could not read limits, admitting: {e}");
                Verdict {
                    throttled: false,
                    detail: Value::Null,
                }
            }
        };
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        *cache = Some((Instant::now(), verdict.clone()));
        verdict
    }
}

async fn poll(tel: &Telemetry) -> Result<Verdict, String> {
    let resp = tel
        .request(reqwest::Method::GET, "/v1/limits/status")
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(verdict_of(&body))
}

/// Read the API's `throttled` flag and name the rules that bind. Pure, so it is testable
/// without a server.
pub fn verdict_of(body: &Value) -> Verdict {
    let throttled = body
        .get("throttled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let binding: Vec<Value> = body
        .get("statuses")
        .and_then(Value::as_array)
        .map(|s| {
            s.iter()
                .filter(|st| {
                    let action = st.get("action").and_then(Value::as_str).unwrap_or("");
                    let breached = st.get("breached").and_then(Value::as_bool).unwrap_or(false);
                    let shed = st
                        .get("shed_fraction")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    matches!(action, "throttle" | "block") && (breached || shed >= 1.0)
                })
                .map(|st| {
                    serde_json::json!({
                        "rule_id": st.get("rule_id"),
                        "metric": st.get("metric"),
                        "window": st.get("window"),
                        "action": st.get("action"),
                        "ratio": st.get("ratio"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Verdict {
        throttled,
        detail: serde_json::json!({ "binding": binding }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_api_flag_decides_and_the_binding_rules_are_named() {
        let v = verdict_of(&json!({
            "throttled": true,
            "statuses": [
                {"rule_id": "r1", "action": "alert", "breached": true, "ratio": 1.2},
                {"rule_id": "r2", "action": "block", "breached": true, "ratio": 1.1,
                 "metric": "cost_usd", "window": "day"},
                {"rule_id": "r3", "action": "throttle", "breached": false, "shed_fraction": 0.3}
            ]
        }));
        assert!(v.throttled);
        let binding = v.detail["binding"].as_array().unwrap();
        assert_eq!(binding.len(), 1);
        assert_eq!(binding[0]["rule_id"], "r2");
    }

    #[test]
    fn an_empty_or_odd_body_admits() {
        assert!(!verdict_of(&json!({})).throttled);
        assert!(!verdict_of(&json!({"throttled": "yes"})).throttled);
    }
}
