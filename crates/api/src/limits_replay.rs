//! The store-facing half of `GET /v1/limits/preview`: turning a candidate rule and a span into the
//! rolling-usage series `lighttrack_core::limits::backtest` replays.
//!
//! Split from `limits_preview.rs` (which owns the HTTP shape) to keep both files inside the repo's
//! size rule, and because this is the half with the safety property worth testing on its own: it
//! **propagates `StoreError::Unsupported` rather than returning a short series**. An empty timeline
//! reads as "nothing would have breached", which is the one answer a backtest must never give when
//! it could not look.

use chrono::{DateTime, Duration, Utc};

use lighttrack_core::limits::backtest::BacktestSample;
use lighttrack_core::{
    Dimension, LimitAction, LimitMetric, LimitRule, LimitScope, LimitWindow, RollupQuery,
    RollupRow, Threshold, TimeKey,
};
use lighttrack_store::{scope_usage::usage_for_scope, Store, StoreError, Usage};

use crate::error::ApiError;

/// Sampled instants per replay. 48 is half-hourly resolution over a `day` window and keeps the work
/// one bounded fan of aggregates rather than an unbounded one.
pub(crate) const DEFAULT_STEPS: usize = 48;
pub(crate) const MAX_STEPS: usize = 200;
/// Ceiling on the *default* look-back, so a `month` window does not silently ask for 210 days.
const MAX_DEFAULT_SPAN_DAYS: i64 = 90;

/// `7d` / `48h` / `90m` / `3600s`, or a bare integer read as seconds.
pub(crate) fn parse_span(s: &str) -> Result<Duration, ApiError> {
    let t = s.trim();
    let (digits, mult) = match t.chars().last() {
        Some('d') => (&t[..t.len() - 1], 86_400),
        Some('h') => (&t[..t.len() - 1], 3_600),
        Some('m') => (&t[..t.len() - 1], 60),
        Some('s') => (&t[..t.len() - 1], 1),
        _ => (t, 1),
    };
    let n: i64 = digits.trim().parse().map_err(|_| {
        ApiError::bad_request(format!(
            "unknown span '{s}' (expected a count with a unit, e.g. 7d / 48h / 90m)"
        ))
    })?;
    if n <= 0 {
        return Err(ApiError::bad_request(format!(
            "span '{s}' must be positive"
        )));
    }
    Ok(Duration::seconds(n * mult))
}

/// How far back to replay when the caller said nothing: seven of the candidate's own windows,
/// capped, so the default answer is about the rule being previewed rather than a fixed guess.
pub(crate) fn default_span(window: LimitWindow) -> Duration {
    (window.lookback() * 7).min(Duration::days(MAX_DEFAULT_SPAN_DAYS))
}

/// The sampled instants, oldest first, with the newest exactly at `to`.
pub(crate) fn instants(from: DateTime<Utc>, to: DateTime<Utc>, steps: usize) -> Vec<DateTime<Utc>> {
    let span = (to - from).num_milliseconds().max(1);
    (1..=steps)
        .map(|i| from + Duration::milliseconds(span * i as i64 / steps as i64))
        .collect()
}

/// Sum rollup rows into a [`Usage`] — the same fields `usage_since` carries, so a step's cost
/// evidence (unpriced calls, the client-reported share) survives into the evaluator.
fn sum(rows: &[RollupRow]) -> Usage {
    let mut u = Usage::default();
    for r in rows {
        u.cost_usd += r.cost_usd;
        u.calls += r.calls as i64;
        u.tokens += r.tokens() as i64;
        u.unpriced_calls += r.unpriced_calls as i64;
        u.client_cost_usd += r.client_reported_cost_usd;
    }
    u
}

/// Rolling usage over `[since, until)` for the candidate's scope.
///
/// The split is not cosmetic: a `model` scope's membership is the canonical family (`gpt-4o` catches
/// `gpt-4o-2024-08-06`), so it goes through the one authority that knows that. A preview that
/// narrowed it back to `WHERE model = ?` would report a confident "nothing would have breached"
/// about a cap that breaches daily — the exact divergence the membership authority was built to
/// close. The exact-value dimensions keep an indexed equality filter, because for them exact *is*
/// the matcher.
fn window_usage(
    store: &(dyn Store + Send + Sync),
    project: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    scope: Option<&LimitScope>,
) -> Result<Usage, StoreError> {
    match scope {
        Some(s) if s.folds_canonical_family() => {
            usage_for_scope(|q| store.rollup(q), project, since, Some(until), s)
        }
        other => {
            let mut q = RollupQuery::new(&[Dimension::Project], since)
                .project(Some(project))
                .until(Some(until))
                .time_key(TimeKey::ReceivedAt);
            if let Some(s) = other {
                let dim = Dimension::parse(s.kind_str())
                    .ok_or(StoreError::Unsupported("that scope dimension"))?;
                q = q.filter(dim, s.value());
            }
            Ok(sum(&store.rollup(&q)?))
        }
    }
}

/// One sample per instant, each measured over the candidate window's own look-back.
pub(crate) fn collect(
    rule: &LimitRule,
    at: &[DateTime<Utc>],
    usage_at: impl Fn(DateTime<Utc>, DateTime<Utc>) -> Result<Usage, StoreError>,
) -> Result<Vec<BacktestSample>, StoreError> {
    let lookback = rule.window.lookback();
    let mut out = Vec::with_capacity(at.len());
    for t in at {
        let u = usage_at(*t - lookback, *t)?;
        out.push(BacktestSample {
            at: *t,
            current: u.metric_value(rule.metric),
            calls: u.calls,
            cost_usd: u.cost_usd,
            cost_evidence: matches!(rule.metric, LimitMetric::CostUsd).then(|| u.cost_evidence()),
        });
    }
    Ok(out)
}

/// Sample the store itself — the production path `collect` is handed.
pub(crate) fn sampler<'a>(
    store: &'a (dyn Store + Send + Sync),
    project: &'a str,
    scope: Option<&'a LimitScope>,
) -> impl Fn(DateTime<Utc>, DateTime<Utc>) -> Result<Usage, StoreError> + 'a {
    move |since, until| window_usage(store, project, since, until, scope)
}

/// The candidate rule, under a non-storable id so nothing downstream mistakes it for a configured
/// one. It is a real [`LimitRule`], which is the point: it goes through `validate` and the
/// production evaluator, so a previewed cap and a created cap are the same cap.
pub(crate) fn candidate(
    project: &str,
    metric: LimitMetric,
    window: LimitWindow,
    threshold: f64,
    action: LimitAction,
    warn_at: Option<f64>,
    scope: Option<LimitScope>,
) -> LimitRule {
    LimitRule {
        id: "preview".into(),
        project_id: project.to_string(),
        metric,
        window,
        threshold: Threshold::Fixed(threshold),
        action,
        enabled: true,
        warn_at,
        scope,
        escalation: None,
        escalated_until: None,
        origin: None,
        expires_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use lighttrack_core::limits::backtest::replay;

    #[test]
    fn spans_parse_with_their_unit_and_refuse_nonsense() {
        // `ApiError` is not `Debug` by design, so a span is compared through its message.
        let span = |s: &str| parse_span(s).map_err(|e| e.to_string());
        assert_eq!(span("7d"), Ok(Duration::days(7)));
        assert_eq!(span("48h"), Ok(Duration::hours(48)));
        assert_eq!(span("90m"), Ok(Duration::minutes(90)));
        assert_eq!(span(" 3600s "), Ok(Duration::hours(1)));
        assert_eq!(span("600"), Ok(Duration::minutes(10)));
        for bad in ["", "d", "-1d", "0h", "a week", "7days"] {
            assert!(parse_span(bad).is_err(), "'{bad}' must not parse");
        }
    }

    /// The default span is about the rule being previewed, and the month window is capped rather
    /// than quietly asking a backend for 210 days of aggregates.
    #[test]
    fn the_default_span_follows_the_window_and_is_capped() {
        assert_eq!(default_span(LimitWindow::Hour), Duration::hours(7));
        assert_eq!(default_span(LimitWindow::Day), Duration::days(7));
        assert_eq!(default_span(LimitWindow::Month), Duration::days(90));
    }

    #[test]
    fn the_grid_ends_at_the_upper_bound_and_is_monotone() {
        let to = Utc::now();
        let at = instants(to - Duration::days(7), to, 48);
        assert_eq!(at.len(), 48);
        assert_eq!(*at.last().unwrap(), to, "the newest step is 'now'");
        assert!(at[0] > to - Duration::days(7));
        assert!(at.windows(2).all(|w| w[0] < w[1]));
    }

    /// CASE 7 (the HTTP half) — the safety property, proven in two asserted links rather than
    /// assumed. Link one: a backend whose `rollup` is unavailable makes `collect` return
    /// `Unsupported`, never a short or empty series. Link two: that error is a 501 `unsupported` on
    /// the wire. The third assertion is the counterfactual it forbids.
    #[test]
    fn a_backend_that_cannot_answer_501s_instead_of_reporting_no_breach() {
        let rule = candidate(
            "p1",
            LimitMetric::CostUsd,
            LimitWindow::Day,
            5.0,
            LimitAction::Block,
            None,
            None,
        );
        let now = Utc::now();
        let at = instants(now - Duration::days(7), now, 8);
        let err = collect(&rule, &at, |_, _| {
            Err(StoreError::Unsupported("the grouped rollup"))
        })
        .expect_err("a missing query must not read as an empty timeline");
        assert!(matches!(err, StoreError::Unsupported(_)));

        let resp = ApiError::from(err).into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_IMPLEMENTED);

        let (t, b) = rule.threshold.resolve(None);
        let swallowed = replay(&rule, t, b, &[]);
        assert!(
            !swallowed.would_breach && swallowed.episodes.is_empty(),
            "this is the reassuring verdict a swallowed Unsupported would have published"
        );
    }

    /// Every scope the API accepts is answerable: a `model` scope through the membership authority,
    /// the rest through a rollup dimension. A scope with neither would 501 at replay time, which is
    /// honest but would mean a published filter nothing serves.
    #[test]
    fn every_scope_dimension_is_answerable_one_way_or_the_other() {
        for kind in LimitScope::KINDS {
            let s: LimitScope =
                serde_json::from_value(serde_json::json!({ *kind: "v" })).expect("scope parses");
            assert!(
                s.folds_canonical_family() || Dimension::parse(s.kind_str()).is_some(),
                "{kind} can be neither folded nor filtered"
            );
        }
        assert!(LimitScope::Model("gpt-4o".into()).folds_canonical_family());
        assert!(!LimitScope::Provider("openai".into()).folds_canonical_family());
    }

    /// A `cost_usd` candidate carries cost provenance into the evaluator; a `calls` candidate has
    /// nothing to qualify and must not invent evidence.
    #[test]
    fn cost_evidence_rides_along_only_for_a_cost_cap() {
        let now = Utc::now();
        let at = instants(now - Duration::hours(2), now, 2);
        let u = Usage {
            cost_usd: 4.0,
            calls: 4,
            tokens: 40,
            unpriced_calls: 2,
            client_cost_usd: 1.0,
        };
        for (metric, want) in [(LimitMetric::CostUsd, true), (LimitMetric::Calls, false)] {
            let r = candidate(
                "p1",
                metric,
                LimitWindow::Day,
                5.0,
                LimitAction::Alert,
                None,
                None,
            );
            let s = collect(&r, &at, |_, _| Ok(u)).expect("sampled");
            assert_eq!(s[0].cost_evidence.is_some(), want, "{metric:?}");
            assert_eq!(s[0].calls, 4);
        }
    }
}
