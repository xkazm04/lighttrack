//! Breach attribution: after a cost/calls/tokens cap trips, name *what drove the spend*. The
//! operator's next question is always "what's burning the money?", so a breach alert carries the top
//! contributors for the breached project over the breached window.
//!
//! Pure composition ([`compose`]) is split from I/O ([`fetch`]) so the top-3 selection, share math,
//! and scoped-rule wording are unit-tested with fixture rows. Everything runs inside the spawned
//! delivery task (zero cost on the ingest path) and is best-effort: an empty or failed rollup simply
//! yields no attribution and the alert still delivers.

use lighttrack_core::{LimitScope, LimitWindow};
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
        .cost_summary_windowed(Some(project), Some(since), None)
        .unwrap_or_default();
    let usecase_rows = store.usecase_costs(Some(project), Some(since)).unwrap_or_default();
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
            Attribution { contributors: rank(items, total), scope_note: None }
        }
        Some(s @ LimitScope::Provider(p)) => {
            let rows = cost_rows.iter().filter(|r| &r.provider == p);
            let total = sum(rows.clone().map(|r| r.cost_usd));
            scoped(rank(group_models(rows, usecase_rows), total), s)
        }
        Some(s @ LimitScope::Model(m)) => {
            // Within a model cap, the contributors are that model's use-cases.
            let rows: Vec<_> = usecase_rows.iter().filter(|r| &r.model == m).collect();
            let total = sum(rows.iter().map(|r| r.cost_usd));
            let items = group_by(rows.iter().map(|r| {
                (r.name.clone().unwrap_or_else(|| "(unnamed)".to_string()), r.cost_usd)
            }));
            scoped(rank(items, total), s)
        }
        Some(s @ LimitScope::Name(n)) => {
            // Within a use-case cap, the contributors are the models serving that use-case.
            let rows: Vec<_> =
                usecase_rows.iter().filter(|r| r.name.as_deref() == Some(n.as_str())).collect();
            let total = sum(rows.iter().map(|r| r.cost_usd));
            let items = group_by(rows.iter().map(|r| (r.model.clone(), r.cost_usd)));
            scoped(rank(items, total), s)
        }
    }
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
        .max_by(|a, b| a.cost_usd.partial_cmp(&b.cost_usd).unwrap_or(std::cmp::Ordering::Equal));
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
            share_pct: if total > 0.0 { cost / total * 100.0 } else { 0.0 },
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
    Attribution { contributors, scope_note: Some(note) }
}

fn sum(iter: impl Iterator<Item = f64>) -> f64 {
    iter.sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cost(provider: &str, model: &str, c: f64) -> CostRow {
        CostRow {
            project_id: "p".into(),
            provider: provider.into(),
            model: model.into(),
            calls: 1,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: c,
        }
    }

    fn uc(name: Option<&str>, provider: &str, model: &str, c: f64) -> UseCaseCostRow {
        UseCaseCostRow {
            name: name.map(|s| s.to_string()),
            provider: provider.into(),
            model: model.into(),
            calls: 1,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: c,
        }
    }

    #[test]
    fn unscoped_ranks_top_three_models_with_shares_and_usecase_annotation() {
        let costs = vec![
            cost("openai", "gpt-4o", 6.0),
            cost("anthropic", "claude-sonnet", 3.0),
            cost("openai", "gpt-4o-mini", 1.0),
            cost("openai", "tiny", 0.0), // zero spend dropped
        ];
        let ucs = vec![
            uc(Some("summarize"), "openai", "gpt-4o", 5.0),
            uc(Some("classify"), "openai", "gpt-4o", 1.0),
            uc(None, "anthropic", "claude-sonnet", 3.0),
        ];
        let a = compose(&costs, &ucs, None);
        assert_eq!(a.scope_note, None);
        assert_eq!(a.contributors.len(), 3);
        // total = 10 → shares 60/30/10; gpt-4o annotated with its dominant named use-case.
        assert_eq!(a.contributors[0].label, "gpt-4o (summarize)");
        assert!((a.contributors[0].share_pct - 60.0).abs() < 1e-9);
        assert_eq!(a.contributors[1].label, "claude-sonnet");
        assert!((a.contributors[1].share_pct - 30.0).abs() < 1e-9);
        assert_eq!(a.contributors[2].label, "gpt-4o-mini");
    }

    #[test]
    fn model_scope_attributes_within_the_model_by_usecase() {
        let ucs = vec![
            uc(Some("summarize"), "openai", "gpt-4o", 7.0),
            uc(Some("chat"), "openai", "gpt-4o", 3.0),
            uc(Some("other"), "openai", "gpt-4o-mini", 99.0), // different model → excluded
        ];
        let a = compose(&[], &ucs, Some(&LimitScope::Model("gpt-4o".into())));
        assert_eq!(a.scope_note.as_deref(), Some("within scope model=gpt-4o"));
        assert_eq!(a.contributors.len(), 2);
        assert_eq!(a.contributors[0].label, "summarize");
        assert!((a.contributors[0].share_pct - 70.0).abs() < 1e-9); // within-scope total = 10
    }

    #[test]
    fn name_scope_attributes_within_the_usecase_by_model() {
        let ucs = vec![
            uc(Some("summarize"), "openai", "gpt-4o", 8.0),
            uc(Some("summarize"), "anthropic", "claude-sonnet", 2.0),
            uc(Some("classify"), "openai", "gpt-4o", 50.0), // different use-case → excluded
        ];
        let a = compose(&[], &ucs, Some(&LimitScope::Name("summarize".into())));
        assert_eq!(a.scope_note.as_deref(), Some("within scope name=summarize"));
        assert_eq!(a.contributors[0].label, "gpt-4o");
        assert!((a.contributors[0].share_pct - 80.0).abs() < 1e-9);
    }

    #[test]
    fn empty_rollups_degrade_silently() {
        let a = compose(&[], &[], None);
        assert!(a.is_empty());
        assert!(a.message_tail().is_none());
        // A scoped breach with no spend still states the scope rather than going blank.
        let scoped = compose(&[], &[], Some(&LimitScope::Provider("openai".into())));
        assert!(!scoped.is_empty());
        assert!(scoped.contributors.is_empty());
        assert!(scoped.message_tail().unwrap().contains("no attributable"));
    }

    #[test]
    fn message_tail_lists_contributors() {
        let costs = vec![cost("openai", "gpt-4o", 3.0), cost("openai", "mini", 1.0)];
        let tail = compose(&costs, &[], None).message_tail().unwrap();
        assert!(tail.contains("gpt-4o 75%"), "got: {tail}");
        assert!(tail.contains("mini 25%"), "got: {tail}");
    }
}
