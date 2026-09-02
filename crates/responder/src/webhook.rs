//! The `/webhook` endpoint: verify the delivery is really from LightTrack, turn its `error_spike`
//! (`spike`) or `score_drop` (`drop`) payload into a [`Trigger`], and hand it to the pipeline on a
//! detached task so we ack the POST immediately (investigations are slow).
//!
//! **Why the signature matters here specifically.** This service runs Claude Code against a local
//! repo and, for opt-in projects, edits files on a branch. An unauthenticated POST to `/webhook` is
//! therefore an unauthenticated way to spend money and touch source. When
//! `LIGHTTRACK_RESPONDER_WEBHOOK_SECRET` is set, a delivery must carry a valid
//! `X-LightTrack-Signature` over the exact bytes received — verified with
//! [`lighttrack_core::alert_sign`], the same code that produced it, because a signature scheme with
//! two implementations is a scheme with two behaviours.
//!
//! The endpoint still answers 200 to a payload it does not act on, because that is a *delivery*
//! success — but it no longer answers 200 to a delivery it rejected: an unsigned or wrongly-signed
//! POST gets 401, so a misconfigured secret is visible on the sending side instead of silently
//! dropping every alert.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::Value;

use crate::pipeline;
use crate::state::AppState;

/// How far a delivery's timestamp may be from now. Generous enough for clock skew between two
/// hosts, tight enough that a captured body is not replayable an hour later.
const SIGNATURE_TOLERANCE_SECS: i64 = 300;

/// The `spike` object LightTrack's alerter emits for an `error_spike` event.
#[derive(Deserialize, Clone)]
pub(crate) struct Spike {
    pub project_id: String,
    #[serde(default)]
    pub count: Option<u32>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// The `drop` object LightTrack's alerter emits for a `score_drop` (quality regression) event.
#[derive(Deserialize, Clone)]
pub(crate) struct Drop {
    pub project_id: String,
    #[serde(default)]
    pub rubric: Option<String>,
    #[serde(default)]
    pub recent_avg: Option<f64>,
    #[serde(default)]
    pub baseline_avg: Option<f64>,
    #[serde(default)]
    pub drop_pct: Option<f64>,
    #[serde(default)]
    pub scored_by: Option<String>,
}

/// What made the responder wake up: a failure spike or a quality regression.
pub(crate) enum Trigger {
    Error(Spike),
    Quality(Drop),
}

impl Trigger {
    pub(crate) fn project_id(&self) -> &str {
        match self {
            Trigger::Error(s) => &s.project_id,
            Trigger::Quality(d) => &d.project_id,
        }
    }
}

/// Takes the raw body, not `Json<Value>`: the signature covers the exact bytes on the wire, and
/// re-serializing a parsed value would change them.
pub(crate) async fn receive(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> StatusCode {
    if let Some(secret) = &st.cfg.webhook_secret {
        let header = headers
            .get(lighttrack_core::alert_sign::SIGNATURE_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !lighttrack_core::alert_sign::verify(
            header,
            secret,
            &body,
            chrono::Utc::now().timestamp(),
            SIGNATURE_TOLERANCE_SECS,
        ) {
            eprintln!(
                "[responder] rejected an unsigned or mis-signed delivery — this endpoint spends \
                 money and can edit a repo, so it does not act on an unverified body"
            );
            return StatusCode::UNAUTHORIZED;
        }
    }

    let body: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return bad("body", e),
    };
    let event = body
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or("(unknown)");
    // Additive field: the ledger row this delivery came from, so the diagnosis can be posted back
    // as its resolution. Absent from a pre-ledger sender, in which case the loop simply stays open.
    let alert_id = body
        .get("alert_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let trigger = if let Some(v) = body.get("spike") {
        match serde_json::from_value::<Spike>(v.clone()) {
            Ok(s) => Trigger::Error(s),
            Err(e) => return bad("spike", e),
        }
    } else if let Some(v) = body.get("drop") {
        match serde_json::from_value::<Drop>(v.clone()) {
            Ok(d) => Trigger::Quality(d),
            Err(e) => return bad("drop", e),
        }
    } else {
        // Breach / forecast / relay-dead alerts share this endpoint; only spikes/drops drive a run.
        println!("[responder] ignoring alert event='{event}' (no spike/drop payload)");
        return StatusCode::OK;
    };
    tokio::spawn(pipeline::handle_trigger(
        st.cfg, st.breaker, trigger, alert_id,
    ));
    StatusCode::OK
}

fn bad(kind: &str, e: serde_json::Error) -> StatusCode {
    eprintln!("[responder] malformed {kind} payload: {e}");
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::http::{HeaderMap, HeaderValue};
    use lighttrack_core::alert_sign::{derive_key, signature_header, SIGNATURE_HEADER};

    use super::*;
    use crate::breaker::Breaker;
    use crate::config::{Config, Defaults};

    const SECRET: &str = "whsec_responder_test";

    fn state(secret: Option<&str>) -> AppState {
        let cfg = Config {
            bind: "127.0.0.1:0".into(),
            lighttrack_url: "http://127.0.0.1:1".into(),
            claude_bin: "claude".into(),
            report_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            defaults: Defaults::fallback(),
            // No mapped projects: an admitted delivery is logged and skipped by the pipeline, so the
            // handler's verdict is the only thing under test.
            projects: HashMap::new(),
            email: None,
            api_key: None,
            webhook_secret: secret.map(str::to_string),
        };
        AppState {
            cfg: Arc::new(cfg),
            breaker: Arc::new(Breaker::new(1)),
        }
    }

    fn signed(body: &str, secret: &str, t: i64) -> HeaderMap {
        let mut h = HeaderMap::new();
        let v = signature_header(Some(&derive_key(secret)), None, t, body).expect("signed");
        h.insert(SIGNATURE_HEADER, HeaderValue::from_str(&v).unwrap());
        h
    }

    const BODY: &str = r#"{"event":"error_spike","spike":{"project_id":"p","count":5}}"#;

    /// The one property that makes this endpoint safe to expose: with a secret configured, an
    /// unsigned, mis-signed or stale delivery is refused with 401 — never quietly accepted as a 200.
    #[tokio::test]
    async fn a_configured_secret_refuses_unsigned_missigned_and_stale_deliveries() {
        let st = state(Some(SECRET));
        let now = chrono::Utc::now().timestamp();
        assert_eq!(
            receive(State(st.clone()), HeaderMap::new(), BODY.into()).await,
            StatusCode::UNAUTHORIZED,
            "unsigned"
        );
        assert_eq!(
            receive(
                State(st.clone()),
                signed(BODY, "whsec_other", now),
                BODY.into()
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "signed with a different secret"
        );
        assert_eq!(
            receive(
                State(st.clone()),
                signed(BODY, SECRET, now - SIGNATURE_TOLERANCE_SECS - 60),
                BODY.into()
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "a captured delivery replayed outside the tolerance"
        );
        assert_eq!(
            receive(State(st.clone()), signed(BODY, SECRET, now), BODY.into()).await,
            StatusCode::OK,
            "a genuine delivery is admitted"
        );
    }

    /// Without a secret the endpoint is deliberately open (loopback deployments) — and stays so, so
    /// that adding this check did not silently turn every unsecured local setup into a 401 wall.
    #[tokio::test]
    async fn no_secret_means_unsigned_deliveries_are_still_admitted() {
        let st = state(None);
        assert_eq!(
            receive(State(st), HeaderMap::new(), BODY.into()).await,
            StatusCode::OK
        );
    }
}
