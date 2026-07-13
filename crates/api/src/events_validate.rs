//! Ingest validation policy — the semantic checks a well-formed event must pass before it is priced,
//! admitted, and stored. Deserialization already guarantees the shape (`provider`/`model` present,
//! types correct); this layer rejects values that are structurally valid but would corrupt downstream
//! math or rollups: an empty model, an unrecognized provider, or a timestamp so far off `now` that it
//! skews rolling-window limit/forecast accounting.
//!
//! The policy is resolved once from the environment (`policy()`), but every rule is a pure method on
//! [`IngestPolicy`] so it is unit-testable without touching process env. Both the single-event and the
//! batch ingest paths validate through the same `validate` entry point.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};

use lighttrack_core::LlmEvent;

/// Env: max allowed absolute skew, in seconds, between an event's `ts` and server `now`. Unset or `0`
/// disables the check (current behavior). A positive value rejects events dated more than that many
/// seconds into the past or future.
const ENV_MAX_TS_SKEW: &str = "LIGHTTRACK_MAX_TS_SKEW_SECS";

/// Env: explicit request body-size limit (bytes) for the single-event ingest route. Over this, axum
/// returns 413 before the handler runs. Unset/invalid → [`DEFAULT_MAX_BODY_BYTES`] (matches axum's
/// historical default, so behavior is unchanged unless an operator tightens or loosens it).
const ENV_MAX_BODY_BYTES: &str = "LIGHTTRACK_MAX_BODY_BYTES";
const DEFAULT_MAX_BODY_BYTES: usize = 2 * 1024 * 1024; // 2 MiB

/// Resolve the single-event ingest body-size cap (bytes) from the environment.
pub(crate) fn body_limit_bytes() -> usize {
    std::env::var(ENV_MAX_BODY_BYTES)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_BODY_BYTES)
}

/// Env: max number of events accepted in one `POST /v1/events/batch`. Over this the whole request is
/// rejected 400 (before any item is processed). Default [`DEFAULT_MAX_BATCH`].
const ENV_MAX_BATCH: &str = "LIGHTTRACK_MAX_BATCH";
const DEFAULT_MAX_BATCH: usize = 500;

/// Env: request body-size cap (bytes) for the batch ingest route → 413. Default 8 MiB (a batch is
/// many events, so it's roomier than the single-event cap).
const ENV_MAX_BATCH_BODY_BYTES: &str = "LIGHTTRACK_MAX_BATCH_BODY_BYTES";
const DEFAULT_MAX_BATCH_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Resolve the max items-per-batch from the environment.
pub(crate) fn max_batch() -> usize {
    std::env::var(ENV_MAX_BATCH)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_BATCH)
}

/// Resolve the batch ingest body-size cap (bytes) from the environment.
pub(crate) fn batch_body_limit_bytes() -> usize {
    std::env::var(ENV_MAX_BATCH_BODY_BYTES)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_BATCH_BODY_BYTES)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct IngestPolicy {
    /// `0` = disabled (no bound). Otherwise the max allowed |ts − now| in seconds.
    max_ts_skew_secs: i64,
}

impl IngestPolicy {
    fn from_env() -> Self {
        let max_ts_skew_secs = std::env::var(ENV_MAX_TS_SKEW)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(0);
        Self { max_ts_skew_secs }
    }

    /// Validate one event against `now`. Returns a human-facing 400 message on the first failing rule.
    pub(crate) fn validate(&self, ev: &LlmEvent, now: DateTime<Utc>) -> Result<(), String> {
        if ev.model.trim().is_empty() {
            return Err("`model` must not be empty".to_string());
        }
        // A `provider` outside the modeled variants deserializes to `Unknown` and is ACCEPTED:
        // observability must ingest traffic from providers we haven't modeled yet (mistral, bedrock,
        // ollama, …). Its cost simply stays unpriced (`cost_usd: null`, no `cost_source`), which is
        // visible rather than silent.
        if self.max_ts_skew_secs > 0 {
            let skew = (ev.ts - now).num_seconds().abs();
            if skew > self.max_ts_skew_secs {
                return Err(format!(
                    "`ts` is {skew}s from server time, beyond the allowed {}s skew window",
                    self.max_ts_skew_secs
                ));
            }
        }
        Ok(())
    }
}

/// The process-wide ingest policy, resolved once from the environment.
pub(crate) fn policy() -> &'static IngestPolicy {
    static POLICY: OnceLock<IngestPolicy> = OnceLock::new();
    POLICY.get_or_init(IngestPolicy::from_env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_core::Provider;
    use serde_json::json;

    fn ev(overrides: serde_json::Value) -> LlmEvent {
        let mut base = json!({
            "provider": "anthropic",
            "model": "claude-haiku-4-5",
            "usage": { "input": 1, "output": 1 }
        });
        base.as_object_mut()
            .unwrap()
            .extend(overrides.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    fn disabled_skew() -> IngestPolicy {
        IngestPolicy { max_ts_skew_secs: 0 }
    }

    #[test]
    fn accepts_a_well_formed_event() {
        let now = Utc::now();
        assert!(disabled_skew().validate(&ev(json!({})), now).is_ok());
    }

    #[test]
    fn rejects_empty_or_whitespace_model() {
        let now = Utc::now();
        for m in ["", "   ", "\t"] {
            let e = ev(json!({ "model": m }));
            let err = disabled_skew().validate(&e, now).unwrap_err();
            assert!(err.contains("model"), "{err}");
        }
    }

    #[test]
    fn accepts_unmodeled_provider() {
        let now = Utc::now();
        // An unmodeled provider string deserializes to `Provider::Unknown` and is accepted —
        // observability must ingest traffic from providers we haven't modeled yet.
        let e = ev(json!({ "provider": "mistral" }));
        assert_eq!(e.provider, Provider::Unknown);
        assert!(disabled_skew().validate(&e, now).is_ok());
    }

    #[test]
    fn ts_skew_disabled_accepts_ancient_and_future_events() {
        let now = Utc::now();
        let ancient = ev(json!({ "ts": "2000-01-01T00:00:00Z" }));
        let future = ev(json!({ "ts": "2099-01-01T00:00:00Z" }));
        assert!(disabled_skew().validate(&ancient, now).is_ok());
        assert!(disabled_skew().validate(&future, now).is_ok());
    }

    #[test]
    fn ts_skew_enforced_rejects_backdated_and_future_events() {
        let pol = IngestPolicy { max_ts_skew_secs: 3600 }; // 1h window
        let now = Utc::now();
        // Within the window: accepted.
        let recent = ev(json!({ "ts": (now - chrono::Duration::minutes(30)).to_rfc3339() }));
        assert!(pol.validate(&recent, now).is_ok());
        // Too far in the past.
        let old = ev(json!({ "ts": (now - chrono::Duration::hours(5)).to_rfc3339() }));
        assert!(pol.validate(&old, now).is_err());
        // Too far in the future.
        let ahead = ev(json!({ "ts": (now + chrono::Duration::hours(5)).to_rfc3339() }));
        assert!(pol.validate(&ahead, now).is_err());
    }
}
