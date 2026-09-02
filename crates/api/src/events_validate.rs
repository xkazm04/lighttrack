//! Ingest validation policy — the semantic checks a well-formed event must pass before it is priced,
//! admitted, and stored. Deserialization already guarantees the shape (`provider`/`model` present,
//! types correct); this layer rejects values that are structurally valid but would corrupt downstream
//! math or rollups: an empty model, an unrecognized provider, or a client timestamp so far off `now`
//! that every time-ordered read over it (traces, listings, `since`/`until` windows) becomes fiction.
//! Rolling-window *accounting* no longer rides on `ts` at all — it keys on the server-stamped
//! `received_at` — so this layer is about data quality, and it is ON by default.
//!
//! The policy is resolved once from the environment (`policy()`), but every rule is a pure method on
//! [`IngestPolicy`] so it is unit-testable without touching process env. Both the single-event and the
//! batch ingest paths validate through the same `validate` entry point.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};

use lighttrack_core::LlmEvent;

use crate::error::{ApiError, ErrorCode};

/// Env: max allowed **symmetric** skew, in seconds, between an event's `ts` and server `now` — sets
/// both bounds at once. `0` explicitly disables the check entirely. Unset ⇒ the asymmetric defaults
/// below apply (the check is ON by default now: an unbounded client clock is a data-integrity hole,
/// even though windowed accounting no longer keys on `ts`).
const ENV_MAX_TS_SKEW: &str = "LIGHTTRACK_MAX_TS_SKEW_SECS";

/// Env: max seconds an event's `ts` may sit **in the future** of server time. Default
/// [`DEFAULT_SKEW_FUTURE_SECS`]. A future `ts` is almost always a wrong client clock — a small
/// tolerance covers ordinary NTP drift and nothing else.
const ENV_MAX_TS_SKEW_FUTURE: &str = "LIGHTTRACK_MAX_TS_SKEW_FUTURE_SECS";
/// Env: max seconds an event's `ts` may sit **in the past**. Default [`DEFAULT_SKEW_PAST_SECS`] —
/// generous, because legitimate backfill and offline-buffered SDK retries are real; it only rules out
/// timestamps that are nonsense (a decade off) rather than merely late.
const ENV_MAX_TS_SKEW_PAST: &str = "LIGHTTRACK_MAX_TS_SKEW_PAST_SECS";

const DEFAULT_SKEW_FUTURE_SECS: i64 = 300; // 5 min
const DEFAULT_SKEW_PAST_SECS: i64 = 7 * 24 * 3600; // 7 days

/// A validation failure: a stable machine-readable code plus human prose. The code is drawn from the
/// one API-wide [`ErrorCode`] taxonomy — the single path returns it in `error.code`, the batch path
/// in the item's `code` — so the two ingest surfaces can never drift apart. The message may be
/// reworded at any time; never parse it.
#[derive(Debug, Clone)]
pub(crate) struct Rejection {
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
}

impl Rejection {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<Rejection> for ApiError {
    fn from(r: Rejection) -> Self {
        ApiError::new(r.code, r.message)
    }
}

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
    /// `0` = this bound disabled. Otherwise the max seconds `ts` may lead server time.
    max_future_secs: i64,
    /// `0` = this bound disabled. Otherwise the max seconds `ts` may lag server time.
    max_past_secs: i64,
}

impl IngestPolicy {
    fn from_env() -> Self {
        // The legacy symmetric knob wins when set (including `0`, which is the explicit "off" switch
        // an operator ingesting historical archives needs).
        if let Some(sym) = env_i64(ENV_MAX_TS_SKEW) {
            return Self {
                max_future_secs: sym.max(0),
                max_past_secs: sym.max(0),
            };
        }
        Self {
            max_future_secs: env_i64(ENV_MAX_TS_SKEW_FUTURE)
                .unwrap_or(DEFAULT_SKEW_FUTURE_SECS)
                .max(0),
            max_past_secs: env_i64(ENV_MAX_TS_SKEW_PAST)
                .unwrap_or(DEFAULT_SKEW_PAST_SECS)
                .max(0),
        }
    }

    /// Validate one event against `now`, returning the first failing rule as a coded [`Rejection`].
    pub(crate) fn validate(&self, ev: &LlmEvent, now: DateTime<Utc>) -> Result<(), Rejection> {
        if ev.model.trim().is_empty() {
            return Err(Rejection::new(
                ErrorCode::BadRequest,
                "`model` must not be empty",
            ));
        }
        // A `provider` outside the modeled variants deserializes to `Unknown` and is ACCEPTED:
        // observability must ingest traffic from providers we haven't modeled yet (mistral, bedrock,
        // ollama, …). Its cost simply stays unpriced (`cost_usd: null`, no `cost_source`), which is
        // visible rather than silent.
        //
        // Skew is a *data-quality* rule, not a budget one: windowed accounting keys on the
        // server-stamped `received_at`, so a skewed `ts` can no longer move a cap. What it still
        // corrupts is every time-ordered read (traces, event listings, `since`/`until` windows), so
        // the two directions get distinct codes — a client can tell "your clock is ahead" from
        // "you're replaying something ancient" without parsing prose.
        let delta = (ev.ts - now).num_seconds();
        if self.max_future_secs > 0 && delta > self.max_future_secs {
            return Err(Rejection::new(
                ErrorCode::TsTooNew,
                format!(
                    "`ts` is {delta}s ahead of server time, beyond the allowed {}s future skew \
                     (see LIGHTTRACK_MAX_TS_SKEW_FUTURE_SECS)",
                    self.max_future_secs
                ),
            ));
        }
        if self.max_past_secs > 0 && -delta > self.max_past_secs {
            return Err(Rejection::new(
                ErrorCode::TsTooOld,
                format!(
                    "`ts` is {}s behind server time, beyond the allowed {}s past skew \
                     (see LIGHTTRACK_MAX_TS_SKEW_PAST_SECS)",
                    -delta, self.max_past_secs
                ),
            ));
        }
        Ok(())
    }
}

fn env_i64(key: &str) -> Option<i64> {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
}

/// The process-wide ingest policy, resolved once from the environment.
pub(crate) fn policy() -> &'static IngestPolicy {
    static POLICY: OnceLock<IngestPolicy> = OnceLock::new();
    POLICY.get_or_init(IngestPolicy::from_env)
}

#[cfg(test)]
mod tests {
    use super::*;
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
        IngestPolicy {
            max_future_secs: 0,
            max_past_secs: 0,
        }
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
            assert_eq!(err.code, ErrorCode::BadRequest);
            assert!(err.message.contains("model"), "{}", err.message);
        }
    }

    #[test]
    fn accepts_unmodeled_provider() {
        let now = Utc::now();
        // An unmodeled provider is accepted *as itself* (M8) — observability must ingest traffic
        // from providers we haven't modeled, and keeping the name is what makes its price row and
        // its limit scope reachable.
        let e = ev(json!({ "provider": "mistral" }));
        assert_eq!(e.provider.as_str(), "mistral");
        assert!(disabled_skew().validate(&e, now).is_ok());
        // Only a genuinely absent/blank provider becomes the `unknown` sentinel.
        assert_eq!(ev(json!({ "provider": "  " })).provider.as_str(), "unknown");
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
    fn ts_skew_enforced_rejects_backdated_and_future_events_with_distinct_codes() {
        let pol = IngestPolicy {
            max_future_secs: 3600,
            max_past_secs: 3600,
        };
        let now = Utc::now();
        // Within the window: accepted.
        let recent = ev(json!({ "ts": (now - chrono::Duration::minutes(30)).to_rfc3339() }));
        assert!(pol.validate(&recent, now).is_ok());
        // Too far in the past / future — separately identifiable, never one blurred "bad ts".
        let old = ev(json!({ "ts": (now - chrono::Duration::hours(5)).to_rfc3339() }));
        assert_eq!(
            pol.validate(&old, now).unwrap_err().code,
            ErrorCode::TsTooOld
        );
        let ahead = ev(json!({ "ts": (now + chrono::Duration::hours(5)).to_rfc3339() }));
        assert_eq!(
            pol.validate(&ahead, now).unwrap_err().code,
            ErrorCode::TsTooNew
        );
    }

    #[test]
    fn skew_bounds_are_asymmetric_by_default() {
        // The shipped default tolerates real backfill but not a clock running ahead.
        let pol = IngestPolicy {
            max_future_secs: DEFAULT_SKEW_FUTURE_SECS,
            max_past_secs: DEFAULT_SKEW_PAST_SECS,
        };
        let now = Utc::now();
        let day_old = ev(json!({ "ts": (now - chrono::Duration::days(1)).to_rfc3339() }));
        assert!(
            pol.validate(&day_old, now).is_ok(),
            "a day-old backfill is legitimate"
        );
        let hour_ahead = ev(json!({ "ts": (now + chrono::Duration::hours(1)).to_rfc3339() }));
        assert_eq!(
            pol.validate(&hour_ahead, now).unwrap_err().code,
            ErrorCode::TsTooNew
        );
        let ancient = ev(json!({ "ts": "2000-01-01T00:00:00Z" }));
        assert_eq!(
            pol.validate(&ancient, now).unwrap_err().code,
            ErrorCode::TsTooOld
        );
    }
}
