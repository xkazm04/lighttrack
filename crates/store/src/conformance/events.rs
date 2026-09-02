//! `Surface::EventsCore` ingest/read semantics, and `Surface::EventFilters`' predicates,
//! keyset paging and scoped/grouped usage — the queries whose trait defaults answer
//! plausible-but-wrong (an unfiltered list, all-time cost, project-wide usage).

use chrono::Utc;
use serde_json::json;

use lighttrack_core::{new_id, LimitScope};

use super::fixtures::sample_event;
use crate::{EventFilter, Result, Store};

pub(super) fn events(store: &dyn Store, pid: &str) -> Result<()> {
    store.insert_event(&sample_event(pid, "claude-haiku-4-5", 100, 50, 0.001))?;
    store.insert_event(&sample_event(pid, "claude-haiku-4-5", 200, 80, 0.002))?;

    let listed = store.list_events(Some(pid), 10)?;
    assert_eq!(listed.len(), 2, "list_events scoped to project");
    assert_eq!(listed[0].project_id, pid);
    assert_eq!(listed[0].tags, vec!["conf".to_string()]);
    assert_eq!(
        listed[0].metadata,
        json!({ "k": "v" }),
        "metadata round-trip"
    );
    assert!(
        listed[0].input.is_some() && listed[0].output.is_some(),
        "payload round-trip"
    );

    let one = store.get_event(&listed[0].id)?.expect("get_event Some");
    assert_eq!(one.id, listed[0].id);
    assert!(
        store.get_event(&new_id())?.is_none(),
        "get_event None for unknown id"
    );

    // Re-inserting an existing id must be a typed Conflict on every backend — not an opaque
    // error (Postgres pre-23505-mapping) and never a silent overwrite (Firestore pre-precondition
    // upsert). The API's 409 / idempotency contract rides this variant.
    match store.insert_event(&one) {
        Err(crate::StoreError::Conflict(_)) => {}
        other => panic!("duplicate insert_event must be Err(Conflict), got {other:?}"),
    }
    assert_eq!(
        store.list_events(Some(pid), 10)?.len(),
        2,
        "duplicate insert persisted nothing"
    );

    let costs = store.cost_summary(Some(pid))?;
    assert_eq!(costs.len(), 1, "one (provider,model) group");
    assert_eq!(costs[0].calls, 2);
    assert_eq!(costs[0].input_tokens, 300);
    assert_eq!(costs[0].output_tokens, 130);
    assert!((costs[0].cost_usd - 0.003).abs() < 1e-9, "cost sum");

    let since = Utc::now() - chrono::Duration::hours(1);
    let u = store.usage_since(pid, since)?;
    assert_eq!(u.calls, 2);
    assert_eq!(u.tokens, 430);
    assert!((u.cost_usd - 0.003).abs() < 1e-9, "usage cost");

    Ok(())
}

/// The scoped and grouped usage reads (`Surface::EventFilters`).
///
/// On its own project: the grouped breakdown asserts that its buckets *sum to the window total*, so
/// it cannot share a project with sections that write events for other reasons.
pub(super) fn scoped_usage(store: &dyn Store) -> Result<()> {
    let pid = &new_id();
    let since = Utc::now() - chrono::Duration::hours(1);
    store.insert_event(&sample_event(pid, "claude-haiku-4-5", 100, 50, 0.001))?;
    store.insert_event(&sample_event(pid, "claude-haiku-4-5", 200, 80, 0.002))?;
    // Per-key attribution. `metadata.api_key_id` is server-stamped at ingest and is the dimension a
    // per-key budget scopes to, so a backend that can't read it turns "cap the staging key" into a
    // cap on nothing (or, if it fell back to project-wide, a cap on everything). Both readings —
    // one key's total and the grouped breakdown — are part of the contract.
    let mut keyed = sample_event(pid, "claude-haiku-4-5", 7, 3, 0.004);
    keyed.metadata = json!({ "api_key_id": "conf-key-1" });
    store.insert_event(&keyed)?;

    let k = store.usage_since_scoped(pid, since, &LimitScope::ApiKey("conf-key-1".into()))?;
    assert_eq!(k.calls, 1, "only the keyed event counts toward that key");
    assert!((k.cost_usd - 0.004).abs() < 1e-9);
    let none =
        store.usage_since_scoped(pid, since, &LimitScope::ApiKey("conf-key-absent".into()))?;
    assert_eq!(
        none.calls, 0,
        "an unknown key has no usage (never the project-wide total)"
    );

    let by_key = store.usage_by_scope(pid, since, "api_key")?;
    let keyed_row = by_key
        .iter()
        .find(|r| r.value.as_deref() == Some("conf-key-1"))
        .expect("the keyed row is present in the breakdown");
    assert_eq!(keyed_row.usage.calls, 1);
    let unattributed = by_key
        .iter()
        .find(|r| r.value.is_none())
        .expect("events with no key fold into one unattributed bucket, they are not dropped");
    assert_eq!(unattributed.usage.calls, 2);
    assert_eq!(
        by_key.iter().map(|r| r.usage.calls).sum::<i64>(),
        3,
        "the breakdown's parts sum to the window's total"
    );
    assert!(
        store.usage_by_scope(pid, since, "not-a-dimension").is_err(),
        "an unknown dimension is an error, not an empty (authoritative-looking) breakdown"
    );
    Ok(())
}

/// Exercises the trait's default-bearing query methods — `list_events_filtered`,
/// `cost_summary_windowed`, `usage_since_scoped`, `usecase_costs` — which the SQLite backend overrides
/// but Postgres/Firestore currently inherit. The inherited defaults return *plausible-but-wrong* data
/// (an unfiltered list, all-time cost, project-wide usage, an empty rollup), so before this section
/// the suite passed a backend that silently answered these wrong. It pins the correct behavior against
/// SQLite and will now fail any backend that hasn't ported these queries — the drift signal the
/// systemic parity gap was missing. Scoped to a fresh project so the window/scope math is deterministic.
/// An **unmodeled** provider survives a round-trip verbatim, and its price row is reachable.
///
/// Before M8 the column stored the literal `unknown` for anything outside a closed enum, so a
/// `mistral/*` price row could never be matched by a `mistral` event — on any backend. Runs in its
/// own project so it cannot disturb the counts [`events`] asserts.
pub(super) fn open_provider_identity(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    let mut ev = sample_event(&pid, "mistral-large", 1_000_000, 0, 0.0);
    ev.provider = "mistral".into();
    ev.cost_usd = None;
    store.insert_event(&ev)?;

    let mut back = store.get_event(&ev.id)?.expect("open-provider event");
    assert_eq!(
        back.provider.as_str(),
        "mistral",
        "an unmodeled provider is stored as itself, not as `unknown`"
    );
    let book = lighttrack_core::PriceBook::from_rows(&[lighttrack_core::ModelPriceRow {
        provider: "mistral".into(),
        model: "mistral-large".into(),
        input_per_mtok: 2.0,
        output_per_mtok: 6.0,
        cached_input_per_mtok: None,
        effective_from: Utc::now(),
        verified_at: None,
        note: None,
        source_url: None,
    }]);
    assert_eq!(
        back.ensure_cost(&book),
        Some(2.0),
        "a mistral event prices from a mistral price row"
    );
    Ok(())
}

/// `Surface::RedactionPosture`: the stamp survives the round-trip through `metadata`, and the report
/// groups by it without folding the three postures into one.
///
/// The bar is the same one the stamp exists to clear. A backend that stored the stamp but grouped
/// unstamped rows in with deliberately-unscrubbed ones would answer "everything is accounted for"
/// about a database half of which nobody can account for.
pub(super) fn redaction_posture(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    let stamp = |v: serde_json::Value| {
        let mut e = sample_event(&pid, "m-red", 1, 1, 0.0);
        if !v.is_null() {
            e.metadata = json!({ "k": "v", "redaction": v });
        }
        e
    };
    let scrubbed = json!({ "policy": "none", "scrub": true, "spans": 4, "rules": "feedfacecafe" });
    store.insert_event(&stamp(serde_json::Value::Null))?;
    store.insert_event(&stamp(
        json!({ "policy": "none", "scrub": false, "spans": 0, "rules": "" }),
    ))?;
    store.insert_event(&stamp(scrubbed.clone()))?;
    store.insert_event(&stamp(scrubbed))?;

    // The stamp must survive storage as a readable object, not as text nobody can parse back.
    let stored = store.list_events(Some(&pid), 10)?;
    assert_eq!(stored.len(), 4);
    let any = stored
        .iter()
        .filter_map(|e| e.redaction())
        .find(|s| s.scrub)
        .expect("a scrubbed stamp round-trips through metadata");
    assert_eq!(any.spans, 4, "span count round-trip");
    assert_eq!(any.rules, "feedfacecafe", "rule fingerprint round-trip");

    let since = Utc::now() - chrono::Duration::hours(1);
    let rows = store.redaction_posture(Some(&pid), since)?;
    let total: u64 = rows.iter().map(|r| r.events).sum();
    assert_eq!(total, 4, "every event lands in exactly one posture group");
    assert_eq!(
        rows.len(),
        3,
        "unstamped / stamped-not-scrubbed / scrubbed are three findings, not one: {rows:?}"
    );
    let unknown = rows
        .iter()
        .find(|r| r.stamp.is_none())
        .expect("the unstamped bucket is reported on its own");
    assert_eq!(unknown.events, 1);
    let scrubbed_row = rows
        .iter()
        .find(|r| r.stamp.as_ref().is_some_and(|s| s.scrub))
        .expect("the scrubbed bucket is reported");
    assert_eq!(scrubbed_row.events, 2, "identical stamps collapse into one");
    assert_eq!(scrubbed_row.stamp.as_ref().expect("stamped").spans, 4);
    assert!(
        rows.iter()
            .any(|r| r.stamp.as_ref().is_some_and(|s| !s.scrub)),
        "a deliberate no-scrub is not folded in with the unknowns"
    );
    Ok(())
}

pub(super) fn parity_gap_methods(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    let now = Utc::now();
    let mk = |model: &str, name: &str, cost: f64, ts: chrono::DateTime<Utc>| {
        let mut e = sample_event(&pid, model, 10, 5, cost);
        e.name = Some(name.into());
        e.ts = ts;
        e
    };
    store.insert_event(&mk("m-a", "gen", 1.0, now))?;
    store.insert_event(&mk("m-b", "summarize", 2.0, now))?;
    store.insert_event(&mk("m-a", "gen", 4.0, now - chrono::Duration::hours(48)))?;

    // list_events_filtered: a model filter must actually filter (the default returns ALL events).
    let filter = EventFilter {
        model: Some("m-b".into()),
        ..Default::default()
    };
    let page = store.list_events_filtered(Some(&pid), &filter, 50)?;
    assert_eq!(
        page.events.len(),
        1,
        "model filter returns only m-b (default would return all 3)"
    );
    assert_eq!(page.events[0].model, "m-b");

    // cost_summary_windowed: a 1h window excludes the 48h-old event (the default returns all-time).
    let since = now - chrono::Duration::hours(1);
    let windowed = store.cost_summary_windowed(Some(&pid), Some(since), None)?;
    let total: f64 = windowed.iter().map(|c| c.cost_usd).sum();
    assert!(
        (total - 3.0).abs() < 1e-9,
        "windowed cost = a+b = 3.0, not all-time 7.0 (got {total})"
    );

    // usage_since_scoped: scoping to model m-b sees only b (the default falls back to project-wide).
    let scoped = store.usage_since_scoped(&pid, since, &LimitScope::Model("m-b".into()))?;
    assert_eq!(
        scoped.calls, 1,
        "scoped usage counts only m-b (default would count both)"
    );
    assert!((scoped.cost_usd - 2.0).abs() < 1e-9);

    // usecase_costs: groups by (name, provider, model) within the window (the default returns empty).
    let uc = store.usecase_costs(Some(&pid), Some(since))?;
    let summarize = uc
        .iter()
        .find(|r| r.name.as_deref() == Some("summarize"))
        .expect("summarize use-case group present (default returns an empty rollup)");
    assert_eq!(summarize.calls, 1);
    assert!((summarize.cost_usd - 2.0).abs() < 1e-9);

    // Keyset paging: 3 events, page size 2 → one continuation page, then exhaustion. No event may
    // be duplicated or skipped across the page boundary (the default mints no cursor at all).
    let page1 = store.list_events_filtered(Some(&pid), &EventFilter::default(), 2)?;
    assert_eq!(page1.events.len(), 2, "first page fills to the limit");
    let cursor = page1
        .next_cursor
        .clone()
        .expect("more rows exist -> next_cursor is minted");
    let page2 = store.list_events_filtered(
        Some(&pid),
        &EventFilter {
            cursor: Some(cursor),
            ..Default::default()
        },
        2,
    )?;
    assert_eq!(
        page2.events.len(),
        1,
        "second page holds the remaining event"
    );
    assert!(
        page2.next_cursor.is_none(),
        "exhausted -> no further cursor"
    );
    let mut ids: Vec<&str> = page1
        .events
        .iter()
        .chain(page2.events.iter())
        .map(|e| e.id.as_str())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(
        ids.len(),
        3,
        "no duplicate or skipped events across the page boundary"
    );

    // Predicates AND-combine: model + name + window jointly isolate the single recent m-a event.
    let filter = EventFilter {
        model: Some("m-a".into()),
        name: Some("gen".into()),
        since: Some(since),
        ..Default::default()
    };
    let anded = store.list_events_filtered(Some(&pid), &filter, 50)?;
    assert_eq!(
        anded.events.len(),
        1,
        "model+name+since AND together (not OR / not ignored)"
    );
    assert_eq!(anded.events[0].model, "m-a");
    Ok(())
}
