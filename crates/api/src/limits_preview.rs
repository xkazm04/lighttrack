//! `GET /v1/limits/preview` — what a candidate cap *would have* done, before it ever returns a 429.
//!
//! `/v1/limits/usage` answers who is spending; `/v1/limits/status` answers where the caps already
//! configured stand. Neither answers the question an operator actually has before writing a rule:
//! *if this threshold had existed last week, when would it have fired, and how much would it have
//! turned away?* So the loop was guess → POST → wait → get paged → adjust.
//!
//! Everything this needs already existed: `RollupQuery` bounds a window on both ends and can be
//! keyed on `received_at` (the accounting clock), `LimitRule::validate` is the door a create goes
//! through, and `LimitRule::evaluate_resolved_at` takes the clock instant. This module is the
//! question, not new arithmetic. It follows `/v1/margin/simulate`'s precedent: a **GET**, so it is
//! side-effect-free by construction — nothing is stored, no rule is created, no alert is raised, no
//! rejection is counted — which is also what lets the MCP tool be `readOnlyHint` rather than gated.
//!
//! Two boundaries held deliberately, both in `limits_replay.rs` where the reads happen:
//!
//! * **`Unsupported` → 501, never an empty timeline.** "Nothing would have breached" and "I could
//!   not look" must not be the same response — the honesty rule `limits_usage.rs` states in its own
//!   header, and the whole safety property of a backtest.
//! * **Scope membership is not rewritten here.** `scope_usage::usage_for_scope` stays the one
//!   authority for the canonical-family fold a `model` cap applies.
//!
//! Monitored ingest only, exactly like the caps it previews: judge and benchmark spend is unbudgeted
//! and never appears here, because a threshold informed by it is a threshold no real cap can hold.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::limits::backtest::{replay, Backtest};
use lighttrack_core::{LimitAction, LimitMetric, LimitScope, LimitWindow};

use crate::error::ApiError;
use crate::guards::{authenticate, resolve_read_project};
use crate::limits_replay as replay_io;
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct PreviewParams {
    project: Option<String>,
    /// `cost_usd` (default) | `calls` | `tokens`.
    metric: Option<String>,
    /// `hour` | `day` (default) | `month` — the candidate rule's rolling window.
    window: Option<String>,
    /// The candidate threshold. Required: there is nothing to preview without one.
    threshold: Option<f64>,
    /// `alert` (default) | `throttle` | `block`.
    action: Option<String>,
    warn_at: Option<f64>,
    /// The candidate dimension scope as JSON, e.g. `{"provider":"openai"}`.
    scope: Option<String>,
    /// How far back to replay: `7d`, `48h`, `90m`. Defaults to seven windows (capped at 90 days).
    over: Option<String>,
    steps: Option<usize>,
}

/// The replayed span and its sampling grid, so a caller can tell a coarse answer from a fine one.
#[derive(Serialize)]
pub(crate) struct PreviewSpan {
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    steps: usize,
    step_secs: i64,
    /// The rolling look-back each step measures over — the candidate window's own duration.
    window_secs: i64,
}

#[derive(Serialize)]
pub(crate) struct PreviewResp {
    project_id: String,
    metric: LimitMetric,
    window: LimitWindow,
    action: LimitAction,
    threshold: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    warn_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<LimitScope>,
    over: PreviewSpan,
    #[serde(flatten)]
    backtest: Backtest,
    /// Always `true` — a read-only what-if; nothing is persisted. The same self-declaration
    /// `/v1/margin/simulate` carries, for the same reason.
    simulated: bool,
    notes: Vec<&'static str>,
}

/// Parse one closed-vocabulary param through the enum that owns it, so a variant added to the enum
/// cannot be silently rejected here and the error always lists what is actually accepted.
fn parse_enum<T: serde::de::DeserializeOwned>(
    raw: Option<&str>,
    default: &'static str,
    what: &'static str,
    all: &[&'static str],
) -> Result<T, ApiError> {
    let v = raw.unwrap_or(default);
    serde_json::from_value::<T>(serde_json::Value::String(v.to_string())).map_err(|_| {
        ApiError::bad_request(format!("unknown {what} '{v}' (expected {})", all.join("|")))
    })
}

pub(crate) async fn preview_limit(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PreviewParams>,
) -> Result<Json<PreviewResp>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?
        .ok_or_else(|| ApiError::bad_request("project is required"))?;

    let metric: LimitMetric = parse_enum(
        q.metric.as_deref(),
        "cost_usd",
        "metric",
        &["cost_usd", "calls", "tokens"],
    )?;
    let window: LimitWindow = parse_enum(
        q.window.as_deref(),
        LimitWindow::default().as_str(),
        "window",
        &LimitWindow::ALL.map(|w| w.as_str()),
    )?;
    let action: LimitAction = parse_enum(
        q.action.as_deref(),
        "alert",
        "action",
        &["alert", "throttle", "block"],
    )?;
    let scope: Option<LimitScope> = match q.scope.as_deref() {
        None => None,
        Some(s) => Some(serde_json::from_str(s).map_err(|e| {
            ApiError::bad_request(format!(
                "scope must be a JSON object like {{\"model\":\"gpt-4o\"}} ({e})"
            ))
        })?),
    };
    let threshold = q.threshold.ok_or_else(|| {
        ApiError::bad_request("threshold is required: there is nothing to preview without one")
    })?;

    // The candidate goes through the SAME door a create does, so a threshold the API would refuse
    // cannot be previewed into looking reasonable. `threshold=0` is a 400 carrying `validate`'s own
    // message, never a 200 whose every ratio is infinity.
    let rule = replay_io::candidate(
        &project, metric, window, threshold, action, q.warn_at, scope,
    );
    rule.validate().map_err(ApiError::bad_request)?;

    let now = Utc::now();
    let span = match q.over.as_deref() {
        Some(s) => replay_io::parse_span(s)?,
        None => replay_io::default_span(window),
    };
    let steps = q
        .steps
        .unwrap_or(replay_io::DEFAULT_STEPS)
        .clamp(2, replay_io::MAX_STEPS);
    let from = now - span;
    let at = replay_io::instants(from, now, steps);

    let store = st.store.clone();
    let (pid, r2, at2) = (project.clone(), rule.clone(), at.clone());
    let samples = spawn_db(move || {
        let scope = r2.scope.clone();
        replay_io::collect(
            &r2,
            &at2,
            replay_io::sampler(store.as_ref(), &pid, scope.as_ref()),
        )
    })
    .await?;

    let (resolved, basis) = rule.threshold.resolve(None);
    Ok(Json(PreviewResp {
        project_id: project,
        metric,
        window,
        action,
        threshold: resolved,
        warn_at: rule.warn_at,
        scope: rule.scope.clone(),
        over: PreviewSpan {
            from,
            to: now,
            steps: at.len(),
            step_secs: span.num_seconds() / steps as i64,
            window_secs: window.lookback().num_seconds(),
        },
        backtest: replay(&rule, resolved, basis, &samples),
        simulated: true,
        notes: NOTES.to_vec(),
    }))
}

const NOTES: &[&str] = &[
    "A replay, not a rule: nothing is stored, no alert is raised and no rejection is counted. \
     Create the cap with POST /v1/projects/:id/limits once the threshold reads right.",
    "Monitored ingest traffic only — the judge and benchmark engine is unbudgeted, so its spend is \
     absent here exactly as it is absent from every cap.",
    "Each step measures its rolling window on `received_at`, the clock admission measures on, so a \
     backdated event cannot slide out of a replayed window either.",
    "The replay sees traffic that WAS accepted. A cap in force would have turned some of it away, \
     so a breach episode is an upper bound on what the window would have held under enforcement.",
    "`cost_usd` steps charge unpriced traffic by the same imputation enforcement uses; a window \
     with no priced call at all is unpriceable, and an enforcing cap refuses ingest there.",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_vocabularies_are_parsed_through_their_enum() {
        // `ApiError` is not `Debug` by design, so each outcome is compared through its message.
        fn got<T: serde::de::DeserializeOwned>(
            raw: Option<&str>,
            default: &'static str,
        ) -> Result<T, String> {
            parse_enum(raw, default, "field", &["hour", "day"]).map_err(|e| e.to_string())
        }
        assert_eq!(got(Some("tokens"), "cost_usd"), Ok(LimitMetric::Tokens));
        assert_eq!(got(None, "cost_usd"), Ok(LimitMetric::CostUsd));
        assert_eq!(got(Some("month"), "day"), Ok(LimitWindow::Month));
        assert_eq!(got(Some("throttle"), "alert"), Ok(LimitAction::Throttle));

        let Err(msg) = got::<LimitWindow>(Some("fortnight"), "day") else {
            panic!("an unknown window must be a 400, not a silent default");
        };
        assert!(
            msg.contains("fortnight") && msg.contains("hour|day"),
            "{msg}"
        );
    }
}
