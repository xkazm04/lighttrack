//! `GET /v1/limits/usage` — rolling usage broken down by one scope dimension.
//!
//! The question `/v1/limits/status` cannot answer: *who* is spending. Status reports one number per
//! **rule**, so until a rule existed there was nothing to look at, and once one breached the only
//! place naming a contributor was the alert payload — which requires a configured alert channel and,
//! for keys and customers, cannot be produced from the model/use-case rollups at all.
//!
//! This surface is the pre-breach and post-breach answer in one shape: per dimension value, the
//! rolling usage over a window, plus every scoped rule that currently binds that value and where it
//! stands. Read-only, project-scoped by the same guard as every other read.
//!
//! Backends that haven't ported the grouped query answer 501 `unsupported` (never an empty list —
//! "nobody spent anything" is exactly the wrong thing to say to an operator mid-incident).

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::{ApiKey, LimitScope, LimitStatus, LimitWindow};
use lighttrack_store::{ScopeUsage, StoreError, Usage};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::state::{spawn_db, AppState};

/// Cap on returned rows, so a project with a long tail of customers can't produce an unbounded body.
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 200;

#[derive(Deserialize)]
pub(crate) struct UsageParams {
    project: Option<String>,
    /// The scope dimension to group by: `api_key` (default) | `customer` | `model` | `provider` |
    /// `name`.
    by: Option<String>,
    /// Rolling window: `hour` | `day` (default) | `month`.
    window: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
pub(crate) struct UsageByScopeResp {
    project_id: String,
    by: String,
    window: LimitWindow,
    since: DateTime<Utc>,
    /// Project-wide totals over the same window, so a row's share is checkable against the whole.
    total: Usage,
    entries: Vec<ScopeEntry>,
    /// Present when the breakdown was truncated to `limit` rows.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
}

#[derive(Serialize)]
pub(crate) struct ScopeEntry {
    /// The dimension value — for `api_key`, the opaque key **id**. `None` is traffic carrying no
    /// value on this dimension (unnamed calls, untagged customers, or events written by an admin/dev
    /// principal, which is not a key and is never attributed to one).
    value: Option<String>,
    /// Human-recognizable name for the value, when the server can resolve one *and* the caller is an
    /// admin. For `api_key` that is the key's name and its non-secret prefix. Project-key callers get
    /// the id only — a key should not be handed a roster of its siblings.
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(flatten)]
    usage: Usage,
    /// This value's share of the window's cost, in percent (0 when the window has no cost).
    cost_share_pct: f64,
    /// Every enabled rule scoped to *this* value, evaluated against *this* value's usage. Empty when
    /// no rule binds it — which is the normal state before an operator writes one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    rules: Vec<LimitStatus>,
}

/// Parse the `window` query param, defaulting to `day`.
fn parse_window(s: Option<&str>) -> Result<LimitWindow, ApiError> {
    match s.unwrap_or("day") {
        "hour" => Ok(LimitWindow::Hour),
        "day" => Ok(LimitWindow::Day),
        "month" => Ok(LimitWindow::Month),
        other => Err(ApiError::bad_request(format!(
            "unknown window '{other}' (expected hour|day|month)"
        ))),
    }
}

pub(crate) async fn usage_by_scope(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<UsageParams>,
) -> Result<Json<UsageByScopeResp>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?
        .ok_or_else(|| ApiError::bad_request("project is required"))?;
    let by = q.by.clone().unwrap_or_else(|| "api_key".to_string());
    if !LimitScope::KINDS.contains(&by.as_str()) {
        return Err(ApiError::bad_request(format!(
            "unknown dimension '{by}' (expected one of {})",
            LimitScope::KINDS.join("|")
        )));
    }
    let window = parse_window(q.window.as_deref())?;
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let may_label = ensure_can_admin(&p).is_ok();

    let now = Utc::now();
    let since = window.since(now);
    let store = st.store.clone();
    let (pid, kind) = (project.clone(), by.clone());
    // One blocking hop: the grouped usage, the project total, the rules, and (for admins asking about
    // keys) the key roster used purely for labels.
    let (rows, total, rules, keys) = spawn_db(move || {
        let rows = store.usage_by_scope(&pid, since, &kind)?;
        let total = store.usage_since(&pid, since)?;
        let rules = store.list_limit_rules(&pid, true)?;
        // Labels are a nicety: an unported backend (`Unsupported`) simply yields none.
        let keys: Vec<ApiKey> = if may_label && kind == "api_key" {
            store.list_api_keys(&pid).unwrap_or_default()
        } else {
            Vec::new()
        };
        Ok::<_, StoreError>((rows, total, rules, keys))
    })
    .await?;

    Ok(Json(compose(
        project, by, window, since, total, rows, &rules, &keys, limit,
    )))
}

/// Pure shaping of the response: rank by cost, truncate, attach labels, and evaluate each value's
/// scoped rules against its own usage. Split from I/O so the ranking/rule-matching is unit-tested.
#[allow(clippy::too_many_arguments)]
fn compose(
    project: String,
    by: String,
    window: LimitWindow,
    since: DateTime<Utc>,
    total: Usage,
    mut rows: Vec<ScopeUsage>,
    rules: &[lighttrack_core::LimitRule],
    keys: &[ApiKey],
    limit: usize,
) -> UsageByScopeResp {
    rows.sort_by(|a, b| {
        b.usage
            .cost_for_limits()
            .partial_cmp(&a.usage.cost_for_limits())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.usage.calls.cmp(&a.usage.calls))
    });
    let truncated = rows.len() > limit;
    rows.truncate(limit);
    let total_cost = total.cost_for_limits();

    let entries = rows
        .into_iter()
        .map(|r| {
            let label = r.value.as_deref().and_then(|v| {
                keys.iter()
                    .find(|k| k.id == v)
                    .map(|k| format!("{} ({})", k.name, k.prefix))
            });
            // A rule binds this row when it is scoped to this dimension AND this value. Evaluated
            // against the row's own usage — the same evaluator `/v1/limits/status` uses, so the two
            // surfaces can't disagree about where a per-key cap stands.
            let rules = rules
                .iter()
                .filter(|rule| {
                    rule.scope.as_ref().is_some_and(|s| {
                        s.kind_str() == by && Some(s.value()) == r.value.as_deref()
                    })
                })
                .map(|rule| lighttrack_store::evaluate_rule(rule, &r.usage))
                .collect();
            ScopeEntry {
                cost_share_pct: if total_cost > 0.0 {
                    r.usage.cost_for_limits() / total_cost * 100.0
                } else {
                    0.0
                },
                value: r.value,
                label,
                usage: r.usage,
                rules,
            }
        })
        .collect();

    UsageByScopeResp {
        project_id: project,
        by,
        window,
        since,
        total,
        entries,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_core::{LimitAction, LimitMetric, LimitRule};

    fn usage(cost: f64, calls: i64) -> Usage {
        Usage {
            cost_usd: cost,
            calls,
            tokens: calls * 10,
            ..Default::default()
        }
    }

    fn row(value: Option<&str>, cost: f64, calls: i64) -> ScopeUsage {
        ScopeUsage {
            value: value.map(str::to_string),
            usage: usage(cost, calls),
        }
    }

    fn key_rule(id: &str, key: &str, threshold: f64) -> LimitRule {
        LimitRule {
            id: id.into(),
            project_id: "p".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Day,
            threshold,
            action: LimitAction::Block,
            enabled: true,
            warn_at: None,
            scope: Some(LimitScope::ApiKey(key.into())),
        }
    }

    fn resp(
        rows: Vec<ScopeUsage>,
        rules: &[LimitRule],
        keys: &[ApiKey],
        limit: usize,
    ) -> UsageByScopeResp {
        let total = usage(
            rows.iter().map(|r| r.usage.cost_usd).sum(),
            rows.iter().map(|r| r.usage.calls).sum(),
        );
        compose(
            "p".into(),
            "api_key".into(),
            LimitWindow::Day,
            Utc::now(),
            total,
            rows,
            rules,
            keys,
            limit,
        )
    }

    #[test]
    fn ranks_by_cost_and_computes_shares() {
        let r = resp(
            vec![
                row(Some("k-small"), 1.0, 1),
                row(Some("k-big"), 9.0, 3),
                row(None, 0.0, 0),
            ],
            &[],
            &[],
            10,
        );
        assert_eq!(r.entries[0].value.as_deref(), Some("k-big"));
        assert!((r.entries[0].cost_share_pct - 90.0).abs() < 1e-9);
        assert!((r.entries[1].cost_share_pct - 10.0).abs() < 1e-9);
        // Unattributed traffic is a row, not a silent omission.
        assert_eq!(r.entries[2].value, None);
        assert!(!r.truncated);
    }

    #[test]
    fn attaches_only_the_rules_scoped_to_that_very_value() {
        let rules = vec![
            key_rule("r-staging", "k-staging", 5.0),
            key_rule("r-prod", "k-prod", 500.0),
            // A model-scoped rule shares no dimension with an api_key breakdown.
            LimitRule {
                scope: Some(LimitScope::Model("gpt-4o".into())),
                ..key_rule("r-model", "x", 1.0)
            },
            // An unscoped project rule binds no single key.
            LimitRule {
                scope: None,
                ..key_rule("r-all", "x", 1.0)
            },
        ];
        let r = resp(
            vec![row(Some("k-staging"), 6.0, 2), row(Some("k-prod"), 3.0, 1)],
            &rules,
            &[],
            10,
        );
        let staging = &r.entries[0];
        assert_eq!(staging.value.as_deref(), Some("k-staging"));
        assert_eq!(staging.rules.len(), 1);
        assert_eq!(staging.rules[0].rule_id, "r-staging");
        assert!(
            staging.rules[0].breached,
            "$6 of a $5 key budget is a breach, named per key"
        );
        let prod = &r.entries[1];
        assert_eq!(prod.rules.len(), 1);
        assert!(
            !prod.rules[0].breached,
            "the prod key is nowhere near its own, larger budget"
        );
    }

    #[test]
    fn labels_are_resolved_only_from_the_supplied_roster() {
        let keys = vec![ApiKey {
            id: "k-staging".into(),
            project_id: "p".into(),
            name: "staging".into(),
            prefix: "ab12cd".into(),
            key_hash: "salt:deadbeef".into(),
            created_at: Utc::now(),
            last_used_at: None,
            revoked: false,
        }];
        let r = resp(
            vec![row(Some("k-staging"), 1.0, 1), row(Some("k-gone"), 1.0, 1)],
            &[],
            &keys,
            10,
        );
        assert_eq!(r.entries[0].label.as_deref(), Some("staging (ab12cd)"));
        assert_eq!(
            r.entries[1].label, None,
            "an unknown id gets no invented label"
        );
        // The stored hash is never anywhere in the payload — the label is name + non-secret prefix.
        let body = serde_json::to_string(&r).unwrap();
        assert!(
            !body.contains("deadbeef"),
            "key material must never reach this surface"
        );
        assert!(!body.contains("key_hash"));
    }

    #[test]
    fn truncation_is_reported_rather_than_silent() {
        let rows = (0..5)
            .map(|i| row(Some(&format!("k{i}")), i as f64, 1))
            .collect();
        let r = resp(rows, &[], &[], 2);
        assert_eq!(r.entries.len(), 2);
        assert!(r.truncated);
    }
}
