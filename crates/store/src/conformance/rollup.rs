//! `Surface::Rollup`: the one grouped primitive, and the nine legacy methods that must agree with it.
//!
//! The property under test is not "rollup returns rows" — it is that a backend which *overrides* a
//! legacy method (SQLite keeps every hand-written `GROUP BY`) and the `rollup`-derived default
//! answer **the same question with the same answer**. Without that assertion the migration is a
//! second source of truth: `/v1/costs` and `/v1/rollup?by=project,provider,model` would drift, and
//! whichever one an operator happened to open would be "the" number.
//!
//! Ordering is normalised before comparing — the legacy methods sort by cost, `rollup` leaves
//! ordering to the caller, and neither ordering is the thing being checked here.

use chrono::{Duration, Utc};

use lighttrack_core::{new_id, Dimension, RollupQuery, TimeKey};

use super::fixtures::{sample_event, tagged_event};
use crate::{rollup_compat, Result, Store};

pub(super) fn rollup(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    let now = Utc::now();
    let since = now - Duration::days(3);
    let until = now + Duration::hours(1);

    // Two customers, two models, two use-cases, one call with no price on the row, and one call
    // tagged to nobody — every bucket the legacy methods each answer part of.
    let mk = |customer: Option<&str>, model: &str, name: &str, cost: Option<f64>| -> Result<()> {
        let mut e = match customer {
            Some(c) => tagged_event(&pid, c, cost.unwrap_or(0.0)),
            None => sample_event(&pid, model, 10, 5, cost.unwrap_or(0.0)),
        };
        e.model = model.into();
        e.name = Some(name.into());
        e.usage.input = 10;
        e.usage.output = 5;
        e.cost_usd = cost;
        e.ts = now - Duration::hours(2);
        e.received_at = e.ts;
        store.insert_event(&e)
    };
    mk(Some("cus-r-a"), "m-1", "summarize", Some(1.0))?;
    mk(Some("cus-r-a"), "m-2", "classify", Some(2.0))?;
    mk(Some("cus-r-b"), "m-1", "summarize", Some(4.0))?;
    // Unpriced: the price book had no entry, so the row carries no cost at all.
    mk(Some("cus-r-a"), "m-3", "summarize", None)?;
    mk(None, "m-1", "summarize", Some(8.0))?;

    // --- the primitive itself ---
    let q = RollupQuery::new(&[Dimension::Customer], since)
        .project(Some(&pid))
        .until(Some(until));
    let rows = store.rollup(&q)?;
    let a = rows
        .iter()
        .find(|r| r.key(0) == Some("cus-r-a"))
        .expect("the tagged customer has a bucket");
    assert_eq!(a.calls, 3, "three calls for cus-r-a: {rows:?}");
    assert!(
        (a.cost_usd - 3.0).abs() < 1e-9,
        "the stored sum, with the unpriced call contributing nothing: {a:?}"
    );
    assert_eq!(
        a.unpriced_calls, 1,
        "the row discloses that its $3.00 is a floor, not a total: {a:?}"
    );
    assert!(
        rows.iter().any(|r| r.key(0).is_none()),
        "untagged traffic folds into a NULL bucket rather than vanishing: {rows:?}"
    );
    assert_eq!(
        rows.iter().map(|r| r.calls).sum::<u64>(),
        5,
        "the parts sum to the window's calls: {rows:?}"
    );

    // A filter scopes the answer; a value with no traffic is empty, never the project total.
    let scoped = store.rollup(
        &RollupQuery::new(&[Dimension::Model], since)
            .project(Some(&pid))
            .until(Some(until))
            .filter(Dimension::Customer, "cus-r-b"),
    )?;
    assert_eq!(scoped.len(), 1, "only cus-r-b's model: {scoped:?}");
    assert!((scoped[0].cost_usd - 4.0).abs() < 1e-9);
    assert!(store
        .rollup(
            &RollupQuery::new(&[Dimension::Model], since)
                .project(Some(&pid))
                .until(Some(until))
                .filter(Dimension::Customer, "cus-nobody"),
        )?
        .is_empty());

    // Day buckets split, on the accounting key.
    let days = store.rollup(
        &RollupQuery::new(&[Dimension::Day], since)
            .project(Some(&pid))
            .until(Some(until))
            .time_key(TimeKey::ReceivedAt),
    )?;
    assert!(
        days.iter().all(|r| r.key(0).is_some_and(|d| d.len() == 10)),
        "a day key is a YYYY-MM-DD bucket: {days:?}"
    );

    // A malformed query is refused, not answered with something plausible.
    assert!(store.rollup(&RollupQuery::new(&[], since)).is_err());
    assert!(store
        .rollup(&RollupQuery::new(
            &[Dimension::Model, Dimension::Model],
            since
        ))
        .is_err());

    equals_the_legacy_methods(store, &pid, since, until)
}

/// Each legacy method, against the same method derived from `rollup`. A backend that overrides one
/// and a backend that inherits it must produce the same rows.
fn equals_the_legacy_methods(
    store: &dyn Store,
    pid: &str,
    since: chrono::DateTime<Utc>,
    until: chrono::DateTime<Utc>,
) -> Result<()> {
    let p = Some(pid);

    let mut native = store.cost_summary_windowed(p, Some(since), Some(until))?;
    let mut derived = rollup_compat::cost_summary_windowed(store, p, Some(since), Some(until))?;
    let key = |r: &crate::CostRow| (r.project_id.clone(), r.provider.clone(), r.model.clone());
    native.sort_by_key(key);
    derived.sort_by_key(key);
    assert_eq!(
        native.len(),
        derived.len(),
        "cost_summary_windowed row count"
    );
    for (n, d) in native.iter().zip(&derived) {
        assert_eq!(key(n), key(d), "cost_summary_windowed keys");
        assert_eq!(
            (n.calls, n.input_tokens, n.output_tokens),
            (d.calls, d.input_tokens, d.output_tokens)
        );
        assert!((n.cost_usd - d.cost_usd).abs() < 1e-9, "{n:?} vs {d:?}");
        assert_eq!(n.unpriced_calls, d.unpriced_calls, "{n:?} vs {d:?}");
    }

    let mut native = store.usecase_costs(p, Some(since))?;
    let mut derived = rollup_compat::usecase_costs(store, p, Some(since))?;
    let key = |r: &crate::UseCaseCostRow| (r.name.clone(), r.provider.clone(), r.model.clone());
    native.sort_by_key(key);
    derived.sort_by_key(key);
    assert_eq!(native.len(), derived.len(), "usecase_costs row count");
    for (n, d) in native.iter().zip(&derived) {
        assert_eq!(key(n), key(d), "usecase_costs keys");
        assert_eq!(n.calls, d.calls);
        assert_eq!(n.unpriced_calls, d.unpriced_calls);
        assert!((n.cost_usd - d.cost_usd).abs() < 1e-9, "{n:?} vs {d:?}");
    }

    for dim in ["customer", "product", "prompt"] {
        let mut native = store.cost_by_dimension(p, dim, since, until)?;
        let mut derived = rollup_compat::cost_by_dimension(store, p, dim, since, until)?;
        native.sort_by(|a, b| a.key.cmp(&b.key));
        derived.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(native.len(), derived.len(), "cost_by_dimension({dim}) rows");
        for (n, d) in native.iter().zip(&derived) {
            assert_eq!((n.key.clone(), n.calls), (d.key.clone(), d.calls), "{dim}");
            assert!(
                (n.cost_usd - d.cost_usd).abs() < 1e-9,
                "{dim}: {n:?} vs {d:?}"
            );
            assert_eq!(n.unpriced_calls, d.unpriced_calls, "{dim}");
        }

        let mut native = store.tokens_by_dimension(p, dim, since, until)?;
        let mut derived = rollup_compat::tokens_by_dimension(store, p, dim, since, until)?;
        native.sort_by(|a, b| a.key.cmp(&b.key));
        derived.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(
            native
                .iter()
                .map(|r| (r.key.clone(), r.tokens))
                .collect::<Vec<_>>(),
            derived
                .iter()
                .map(|r| (r.key.clone(), r.tokens))
                .collect::<Vec<_>>(),
            "tokens_by_dimension({dim})"
        );
    }

    for kind in ["provider", "model", "name", "customer", "api_key"] {
        let mut native = store.usage_by_scope(pid, since, kind)?;
        let mut derived = rollup_compat::usage_by_scope(store, pid, since, kind)?;
        native.sort_by(|a, b| a.value.cmp(&b.value));
        derived.sort_by(|a, b| a.value.cmp(&b.value));
        assert_eq!(native.len(), derived.len(), "usage_by_scope({kind}) rows");
        for (n, d) in native.iter().zip(&derived) {
            assert_eq!(n.value, d.value, "usage_by_scope({kind}) keys");
            assert_eq!(
                (n.usage.calls, n.usage.tokens, n.usage.unpriced_calls),
                (d.usage.calls, d.usage.tokens, d.usage.unpriced_calls),
                "usage_by_scope({kind})"
            );
            assert!((n.usage.cost_usd - d.usage.cost_usd).abs() < 1e-9);
        }
    }
    // An unknown dimension is refused on both paths — never answered with customer data.
    assert!(store.usage_by_scope(pid, since, "not-a-dimension").is_err());
    assert!(rollup_compat::usage_by_scope(store, pid, since, "not-a-dimension").is_err());

    let mut native = store.daily_usage(pid, since, until)?;
    let mut derived = rollup_compat::daily_usage(store, pid, since, until)?;
    native.sort_by(|a, b| a.day.cmp(&b.day));
    derived.sort_by(|a, b| a.day.cmp(&b.day));
    assert_eq!(
        native
            .iter()
            .map(|r| (r.day.clone(), r.calls, r.tokens))
            .collect::<Vec<_>>(),
        derived
            .iter()
            .map(|r| (r.day.clone(), r.calls, r.tokens))
            .collect::<Vec<_>>(),
        "daily_usage"
    );

    let mut native = store.daily_cost_by_dimension(p, "customer", since, until)?;
    let mut derived = rollup_compat::daily_cost_by_dimension(store, p, "customer", since, until)?;
    let key = |r: &crate::DailyDimCost| (r.day.clone(), r.key.clone());
    native.sort_by_key(key);
    derived.sort_by_key(key);
    assert_eq!(
        native.iter().map(key).collect::<Vec<_>>(),
        derived.iter().map(key).collect::<Vec<_>>(),
        "daily_cost_by_dimension"
    );

    for customer in ["cus-r-a", "cus-r-b", "cus-nobody"] {
        let native = store.customer_cost_by_model(p, customer, since, until)?;
        let derived = rollup_compat::customer_cost_by_model(store, p, customer, since, until)?;
        assert_eq!(
            native
                .iter()
                .map(|r| (r.key.clone(), r.calls))
                .collect::<Vec<_>>(),
            derived
                .iter()
                .map(|r| (r.key.clone(), r.calls))
                .collect::<Vec<_>>(),
            "customer_cost_by_model({customer})"
        );

        let native = store.customer_cost_by_name(p, customer, since, until)?;
        let derived = rollup_compat::customer_cost_by_name(store, p, customer, since, until)?;
        assert_eq!(
            native
                .iter()
                .map(|r| (r.key.clone(), r.calls))
                .collect::<Vec<_>>(),
            derived
                .iter()
                .map(|r| (r.key.clone(), r.calls))
                .collect::<Vec<_>>(),
            "customer_cost_by_name({customer})"
        );
    }
    Ok(())
}
