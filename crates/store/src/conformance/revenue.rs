//! Revenue records and the cost-by-dimension rollup underneath the margin surfaces.

use chrono::Utc;

use lighttrack_core::{compute_margin, new_id, MarginDimension, RevenueEvent, RevenueKind};

use super::fixtures::tagged_event;
use crate::{Result, Store};

/// Revenue + margin (Phase 1 profit tracking). This is the check that catches a backend silently
/// inheriting the trait's no-op revenue defaults (e.g. a backend with no `revenue.rs`): a no-op
/// `insert_revenue_event` errors here, and a no-op `list`/`cost_by_dimension` returns empty and trips
/// the round-trip assertions. Scoped to a fresh project so `cost_by_dimension` (which reads event
/// metadata over a window) sees only the traffic this check inserts.
///
/// It also pins the **idempotent-upsert** invariant: a redelivered webhook — a fresh record sharing
/// the deterministic `stripe:<external_id>` id `normalize_invoice` mints — must upsert onto the
/// existing row, so revenue and every margin number derived from it is recognized exactly once. A
/// backend that keyed off a surrogate row id instead would double-count, and this check fails it.
pub(super) fn revenue(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    // Monitored traffic for two customers: `heavy` is the money-loser.
    store.insert_event(&tagged_event(&pid, "acme", 0.50))?;
    store.insert_event(&tagged_event(&pid, "acme", 0.37))?;
    store.insert_event(&tagged_event(&pid, "heavy", 142.5))?;

    let now = Utc::now();
    // Mirror `billing::normalize_invoice`: a synced record carries a *deterministic* id derived from
    // its external (provider) id — `stripe:<external_id>` — which is the key a redelivered webhook
    // collapses onto. Building ids this way lets the replay below exercise the real idempotency path
    // rather than the trivial re-insert-the-same-struct case.
    let mk_rev = |customer: &str, amount: f64| {
        let external_id = format!("inv-{customer}");
        RevenueEvent {
            id: format!("stripe:{external_id}"),
            project_id: pid.clone(),
            source: "stripe".into(),
            external_id: Some(external_id),
            customer_id: Some(customer.into()),
            product_id: None,
            amount_usd: amount,
            currency: "USD".into(),
            kind: RevenueKind::OneTime,
            period_start: None,
            period_end: None,
            ts: now,
        }
    };
    // The batch path (atomic on backends that override it, a per-record loop otherwise).
    store.insert_revenue_events(&[mk_rev("acme", 20.0), mk_rev("heavy", 99.0)])?;

    let since = now - chrono::Duration::hours(1);
    let until = now + chrono::Duration::hours(1);

    let listed = store.list_revenue_events(Some(&pid), since, until)?;
    assert_eq!(
        listed.len(),
        2,
        "both point-in-time revenue records recognized in window"
    );
    assert!(
        listed.iter().all(|r| r.project_id == pid),
        "list scoped to project"
    );
    let got_acme = listed
        .iter()
        .find(|r| r.customer_id.as_deref() == Some("acme"))
        .expect("acme revenue present");
    assert!(
        (got_acme.amount_usd - 20.0).abs() < 1e-9,
        "amount round-trip"
    );
    assert_eq!(
        got_acme.external_id.as_deref(),
        Some("inv-acme"),
        "external_id round-trip"
    );
    assert_eq!(got_acme.kind, RevenueKind::OneTime, "kind round-trip");

    // A replayed Stripe webhook: `normalize_invoice` runs again on the redelivery and yields a *fresh*
    // record carrying the same deterministic id (`stripe:<external_id>`). The upsert must collapse it
    // onto the existing row — a second physical row here would silently double every downstream margin
    // number, the exact corruption profit tracking exists to prevent.
    store.insert_revenue_event(&mk_rev("acme", 20.0))?;
    let after = store.list_revenue_events(Some(&pid), since, until)?;
    assert_eq!(
        after.len(),
        2,
        "redelivered webhook upserts; total revenue row count unchanged"
    );
    assert_eq!(
        after
            .iter()
            .filter(|r| r.external_id.as_deref() == Some("inv-acme"))
            .count(),
        1,
        "acme stays a single row after replay — no double-count",
    );

    // Cost grouped by the billing dimension, read from event metadata.
    let costs = store.cost_by_dimension(Some(&pid), "customer", since, until)?;
    let acme_cost = costs
        .iter()
        .find(|c| c.key.as_deref() == Some("acme"))
        .expect("acme cost group");
    assert_eq!(acme_cost.calls, 2);
    assert!(
        (acme_cost.cost_usd - 0.87).abs() < 1e-9,
        "acme cost summed across its events"
    );
    let heavy_cost = costs
        .iter()
        .find(|c| c.key.as_deref() == Some("heavy"))
        .expect("heavy cost group");
    assert_eq!(heavy_cost.calls, 1);
    assert!((heavy_cost.cost_usd - 142.5).abs() < 1e-9);

    // End-to-end over the post-replay set: the unprofitable customer surfaces first (margin ascending),
    // and acme's $20 is recognized exactly once despite the redelivery.
    let rows = compute_margin(&after, &costs, MarginDimension::Customer, since, until);
    assert_eq!(rows[0].key, "heavy", "money-loser sorts first");
    assert!((rows[0].gross_margin_usd - (99.0 - 142.5)).abs() < 1e-6);
    let acme_row = rows
        .iter()
        .find(|r| r.key == "acme")
        .expect("acme margin row");
    assert!(
        (acme_row.revenue_usd - 20.0).abs() < 1e-9,
        "revenue recognized once, not doubled"
    );
    assert!(
        (acme_row.gross_margin_usd - 19.13).abs() < 1e-9,
        "revenue − attributed cost"
    );
    Ok(())
}
