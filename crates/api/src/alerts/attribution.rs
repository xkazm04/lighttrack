//! Breach attribution: after a cost/calls/tokens cap trips, name *what drove the spend*. The
//! operator's next question is always "what's burning the money?", so a breach alert carries the top
//! contributors for the breached project over the breached window.
//!
//! Pure composition ([`compose`]) is split from I/O ([`fetch`]) so the top-3 selection, share math,
//! and scoped-rule wording are unit-tested with fixture rows. Everything runs inside the spawned
//! delivery task (zero cost on the ingest path) and is best-effort: an empty or failed rollup simply
//! yields no attribution and the alert still delivers.

use lighttrack_core::{LimitScope, LimitWindow};
use lighttrack_store::Scope as TenantScope;
use lighttrack_store::{CostRow, Store, UseCaseCostRow};
use serde_json::{json, Value};

/// One contributor to the breached window's spend: a model (optionally annotated with its dominant
/// use-case) or, for a scoped rule, a contributor *within* the scope.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Contributor {
    pub(crate) label: String,
    pub(crate) cost_usd: f64,
    /// Share of the (window, or scope) spend, in percent.
    pub(crate) share_pct: f64,
}

/// Up to three top contributors plus, for a scoped rule, a note stating the scope they were computed
/// within (or that the scope had no attributable spend).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Attribution {
    pub(crate) contributors: Vec<Contributor>,
    pub(crate) scope_note: Option<String>,
}

impl Attribution {
    /// Nothing worth attaching: no contributors and no scope note.
    pub(crate) fn is_empty(&self) -> bool {
        self.contributors.is_empty() && self.scope_note.is_none()
    }

    /// A human sentence appended to the breach message, or `None` when there's nothing to say.
    pub(crate) fn message_tail(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let scope = self.scope_note.as_deref().unwrap_or("in this window");
        if self.contributors.is_empty() {
            return Some(format!(" Top spenders: none attributable ({scope})."));
        }
        let list = self
            .contributors
            .iter()
            .map(|c| format!("{} {:.0}% (${:.4})", c.label, c.share_pct, c.cost_usd))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(" Top spenders ({scope}): {list}."))
    }

    /// Structured contributors for the webhook payload.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "scope_note": self.scope_note,
            "contributors": self.contributors.iter().map(|c| json!({
                "label": c.label, "cost_usd": c.cost_usd, "share_pct": c.share_pct,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Fetch the breached window's rollups from `store` and compose attribution. Best-effort: a store
/// error degrades to empty (no attribution), never propagates.
pub(crate) fn fetch(
    store: &dyn Store,
    project: &str,
    window: LimitWindow,
    now: chrono::DateTime<chrono::Utc>,
    scope: Option<&LimitScope>,
) -> Attribution {
    let since = window.since(now);
    let cost_rows = store
        .cost_summary_windowed(TenantScope::Project(project), Some(since), None)
        .unwrap_or_default();
    let usecase_rows = store
        .usecase_costs(TenantScope::Project(project), Some(since))
        .unwrap_or_default();
    compose(&cost_rows, &usecase_rows, scope)
}

/// Pure composition of the top contributors from pre-fetched rollup rows. Unscoped rules attribute
/// across models (annotated with each model's dominant use-case); scoped rules attribute *within*
/// the scope — a model cap by its use-cases, a use-case cap by its models, a provider cap by its
/// models — and carry a note naming the scope.
pub(crate) fn compose(
    cost_rows: &[CostRow],
    usecase_rows: &[UseCaseCostRow],
    scope: Option<&LimitScope>,
) -> Attribution {
    match scope {
        None => {
            let total = sum(cost_rows.iter().map(|r| r.cost_usd));
            let items = group_models(cost_rows.iter(), usecase_rows);
            Attribution {
                contributors: rank(items, total),
                scope_note: None,
            }
        }
        Some(s @ LimitScope::Provider(p)) => {
            let rows = cost_rows.iter().filter(|r| &r.provider == p);
            let total = sum(rows.clone().map(|r| r.cost_usd));
            scoped(rank(group_models(rows, usecase_rows), total), s)
        }
        Some(s @ LimitScope::Model(m)) => {
            // Within a model cap, the contributors are that model's use-cases. Membership is the
            // scope's own rule (`LimitScope::matches`): a cap on `gpt-4o` also catches the dated
            // `gpt-4o-2024-08-06`, so the attribution must fold those rows in too — filtering on
            // the raw string reported "no attributable spend" for exactly the traffic that breached.
            let rows: Vec<_> = usecase_rows
                .iter()
                .filter(|r| same_model(&r.provider, &r.model, m))
                .collect();
            let total = sum(rows.iter().map(|r| r.cost_usd));
            let items = group_by(rows.iter().map(|r| {
                (
                    r.name.clone().unwrap_or_else(|| "(unnamed)".to_string()),
                    r.cost_usd,
                )
            }));
            scoped(rank(items, total), s)
        }
        Some(s @ LimitScope::Name(n)) => {
            // Within a use-case cap, the contributors are the models serving that use-case.
            let rows: Vec<_> = usecase_rows
                .iter()
                .filter(|r| r.name.as_deref() == Some(n.as_str()))
                .collect();
            let total = sum(rows.iter().map(|r| r.cost_usd));
            let items = group_by(rows.iter().map(|r| (r.model.clone(), r.cost_usd)));
            scoped(rank(items, total), s)
        }
        Some(s @ (LimitScope::ApiKey(_) | LimitScope::Customer(_))) => {
            // Deliberately NOT attributed inside the alert. The cost rollups this module reads are
            // grouped by model/use-case and cannot be filtered to one key or customer, and an alert
            // channel is the wrong place to enumerate key identifiers anyway — it fans out to
            // whoever holds the webhook. The operator gets the scope (their own rule) plus a pointer
            // to the authenticated, project-scoped surface that does answer it.
            Attribution {
                contributors: Vec::new(),
                scope_note: Some(format!(
                    "scope {}: per-key/customer breakdown at GET /v1/limits/usage",
                    s.label()
                )),
            }
        }
    }
}

/// Does a rollup row's model fall under a model cap? The same test the admission path applies
/// (`LimitScope::matches`): exact, or the same canonical family once dated suffixes and provider
/// prefixes are stripped. One rule, evaluated twice, must agree — otherwise the breach the cap
/// detected is not the spend the alert explains.
fn same_model(provider: &str, model: &str, capped: &str) -> bool {
    model == capped
        || lighttrack_core::model_id::canonicalize(provider, model).family
            == lighttrack_core::model_id::canonicalize(provider, capped).family
}

/// Aggregate cost rows by model, labelling each model with its dominant (highest-cost) named
/// use-case when one exists — e.g. `gpt-4o (summarize)`.
fn group_models<'a>(
    rows: impl Iterator<Item = &'a CostRow>,
    usecase_rows: &[UseCaseCostRow],
) -> Vec<(String, f64)> {
    let grouped = group_by(rows.map(|r| (r.model.clone(), r.cost_usd)));
    grouped
        .into_iter()
        .map(|(model, cost)| (annotate(&model, usecase_rows), cost))
        .collect()
}

/// The model's dominant named use-case, if any, appended as `model (use-case)`.
fn annotate(model: &str, usecase_rows: &[UseCaseCostRow]) -> String {
    let top = usecase_rows
        .iter()
        .filter(|r| r.model == model && r.name.is_some())
        .max_by(|a, b| {
            a.cost_usd
                .partial_cmp(&b.cost_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    match top.and_then(|r| r.name.as_deref()) {
        Some(name) => format!("{model} ({name})"),
        None => model.to_string(),
    }
}

/// Fold `(label, cost)` pairs into per-label totals (labels may repeat across providers).
fn group_by(items: impl Iterator<Item = (String, f64)>) -> Vec<(String, f64)> {
    let mut map: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();
    for (label, cost) in items {
        *map.entry(label).or_default() += cost;
    }
    map.into_iter().collect()
}

/// Sort by cost desc, keep the top 3 with positive spend, and compute each one's share of `total`.
fn rank(mut items: Vec<(String, f64)>, total: f64) -> Vec<Contributor> {
    items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    items
        .into_iter()
        .filter(|(_, c)| *c > 0.0)
        .take(3)
        .map(|(label, cost)| Contributor {
            label,
            cost_usd: cost,
            share_pct: if total > 0.0 {
                cost / total * 100.0
            } else {
                0.0
            },
        })
        .collect()
}

/// Wrap ranked contributors with a note naming the scope they were computed within.
fn scoped(contributors: Vec<Contributor>, scope: &LimitScope) -> Attribution {
    let label = scope.label();
    let note = if contributors.is_empty() {
        format!("scope {label}: no attributable spend in window")
    } else {
        format!("within scope {label}")
    };
    Attribution {
        contributors,
        scope_note: Some(note),
    }
}

fn sum(iter: impl Iterator<Item = f64>) -> f64 {
    iter.sum()
}

#[cfg(test)]
mod tests;
