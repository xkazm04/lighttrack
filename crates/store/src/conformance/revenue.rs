//! Revenue records and the cost-by-dimension rollup underneath the margin surfaces.

use chrono::Utc;

use lighttrack_core::{compute_margin, new_id, MarginDimension, RevenueEvent, RevenueKind};

use super::fixtures::tagged_event;
use crate::Scope;
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
            // The provider's own figure in minor units, as the adapters now record it. Present here
            // because the redelivery guard below turns on it: without it there is nothing to
            // compare and every retry may restate the row.
            amount_minor: Some((amount * 100.0).round() as i64),
            fx_rate: Some(1.0),
            fx_book_version: Some("conformance-book".into()),
            converted: Some(true),
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

    let listed = store.list_revenue_events(Scope::Project(&pid), since, until)?;
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
    let after = store.list_revenue_events(Scope::Project(&pid), since, until)?;
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

    // FX provenance survives storage: `amount_usd` is derived, so a backend that dropped the
    // original minor-unit figure and the rate behind it would leave a wrong conversion permanently
    // un-correctable — the row could never be repriced, only re-ingested.
    assert_eq!(
        got_acme.amount_minor,
        Some(2000),
        "the provider's own minor-unit figure round-trips"
    );
    assert_eq!(got_acme.fx_rate, Some(1.0), "fx rate round-trip");
    assert_eq!(
        got_acme.fx_book_version.as_deref(),
        Some("conformance-book"),
        "which book produced the rate round-trips"
    );
    assert_eq!(got_acme.converted, Some(true), "converted round-trip");
    assert!(got_acme.is_converted());

    // A redelivery whose minor-unit figure is unchanged must NOT restate the money. The same
    // webhook arriving a month later re-runs the conversion against a *different* FX table, and
    // before this guard that silently moved historical revenue with nothing recording it.
    let mut restated = mk_rev("acme", 20.0);
    restated.amount_usd = 999.0;
    restated.fx_rate = Some(49.95);
    restated.fx_book_version = Some("a-later-book".into());
    store.insert_revenue_event(&restated)?;
    let after_replay = store.list_revenue_events(Scope::Project(&pid), since, until)?;
    let acme_now = after_replay
        .iter()
        .find(|r| r.customer_id.as_deref() == Some("acme"))
        .expect("acme still present");
    assert!(
        (acme_now.amount_usd - 20.0).abs() < 1e-9,
        "an unchanged charge keeps its stored conversion; got {}",
        acme_now.amount_usd
    );
    assert_eq!(
        acme_now.fx_book_version.as_deref(),
        Some("conformance-book"),
        "and keeps the book version that actually produced it"
    );

    // Cost grouped by the billing dimension, read from event metadata.
    let costs = store.cost_by_dimension(Scope::Project(&pid), "customer", since, until)?;
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

/// `Surface::RevenueReprice`: correcting a missing FX rate after the fact.
///
/// The bar is that a reprice is **surgical**. It must move the rows that took the 1:1 fallback and
/// nothing else — a pass that also re-based genuinely-converted rows would restate recognized
/// revenue, which is the failure the redelivery guard above exists to prevent, and this door must
/// not be a way around it. A dry run must report exactly what the real run will do, or nobody can
/// use it to decide.
pub(super) fn reprice(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    let now = Utc::now();
    let mk = |id: &str, currency: &str, minor: Option<i64>, usd: f64, converted: Option<bool>| {
        RevenueEvent {
            id: format!("{pid}:{id}"),
            project_id: pid.clone(),
            source: "stripe".into(),
            external_id: Some(id.into()),
            customer_id: Some("acme".into()),
            product_id: None,
            amount_usd: usd,
            currency: currency.into(),
            amount_minor: minor,
            fx_rate: converted.and_then(|c| c.then_some(1.0)),
            fx_book_version: Some("usd-only".into()),
            converted,
            kind: RevenueKind::OneTime,
            period_start: None,
            period_end: None,
            ts: now,
        }
    };
    // Two GBP invoices stored at the 1:1 fallback (£100 read as $100), one already-converted USD
    // row, one GBP row from before FX provenance existed (no minor-unit figure to re-multiply).
    store.insert_revenue_events(&[
        mk("gbp-1", "GBP", Some(10_000), 100.0, Some(false)),
        mk("gbp-2", "GBP", Some(5_000), 50.0, Some(false)),
        mk("usd-1", "USD", Some(2_000), 20.0, Some(true)),
        mk("gbp-legacy", "GBP", None, 70.0, Some(false)),
    ])?;

    let dry = store.reprice_revenue(Scope::Project(&pid), "GBP", 1.27, "2026-09-02", true)?;
    assert!(dry.dry_run);
    assert_eq!(dry.matched, 3, "every unconverted GBP row matches");
    assert_eq!(
        dry.changed, 2,
        "…but the row with no minor-unit figure cannot be repriced, and says so"
    );

    let since = now - chrono::Duration::hours(1);
    let until = now + chrono::Duration::hours(1);
    let before = store.list_revenue_events(Scope::Project(&pid), since, until)?;
    assert!(
        before
            .iter()
            .all(|r| r.fx_book_version.as_deref() == Some("usd-only")),
        "a dry run writes nothing"
    );

    let run = store.reprice_revenue(Scope::Project(&pid), "gbp", 1.27, "2026-09-02", false)?;
    assert!(!run.dry_run);
    assert_eq!(
        (run.matched, run.changed),
        (dry.matched, dry.changed),
        "the dry run predicted the real one exactly"
    );

    let rows = store.list_revenue_events(Scope::Project(&pid), since, until)?;
    let by = |suffix: &str| {
        rows.iter()
            .find(|r| r.id.ends_with(suffix))
            .unwrap_or_else(|| panic!("row {suffix} present"))
    };
    let gbp1 = by("gbp-1");
    assert!(
        (gbp1.amount_usd - 127.0).abs() < 1e-6,
        "£100 at 1.27 = $127, got {}",
        gbp1.amount_usd
    );
    assert_eq!(gbp1.fx_rate, Some(1.27));
    assert_eq!(gbp1.fx_book_version.as_deref(), Some("2026-09-02"));
    assert_eq!(gbp1.converted, Some(true), "no longer a 1:1 fallback");

    let usd = by("usd-1");
    assert!(
        (usd.amount_usd - 20.0).abs() < 1e-9,
        "an already-converted row is recognized revenue and is left alone"
    );
    assert_eq!(usd.fx_book_version.as_deref(), Some("usd-only"));

    let legacy = by("gbp-legacy");
    assert!(
        (legacy.amount_usd - 70.0).abs() < 1e-9,
        "no original figure to re-multiply -> untouched rather than guessed at"
    );
    assert_eq!(
        legacy.converted,
        Some(false),
        "and still flagged approximate"
    );

    // A currency nobody stored is not an error and not a silent success: zero matched, zero changed.
    let none = store.reprice_revenue(Scope::Project(&pid), "JPY", 0.0068, "2026-09-02", false)?;
    assert_eq!((none.matched, none.changed), (0, 0));
    Ok(())
}
