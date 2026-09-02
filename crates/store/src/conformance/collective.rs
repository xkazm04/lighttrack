//! `Surface::Collective`: the opt-in shared leaderboard's entry table.
//!
//! The three properties the network effect rests on: an entry is **keyed** on
//! (contributor, provider, model, task_type) so a re-send updates rather than double-counts a
//! contributor's weight; a contributor can **withdraw** everything they sent; and the retention
//! purge is **time-bounded** — it must not take entries newer than the cutoff with it.
//!
//! On top of those, the hub-grade trio: a **replace** makes the contributor's stored set exactly the
//! set that was sent (an empty replace is a withdrawal), the **receipt** is that contributor's own
//! newest timestamp, and the pushed-down **retention** predicate answers the same rows the handler
//! would have kept in memory. Whether a replace is one atomic unit is reported by `ReplaceAck` and
//! asserted where the backend's own tests can induce a failure (`sqlite/collective.rs`); a store
//! reached over HTTP cannot be made to fail mid-write from here.

use chrono::{Duration, Utc};

use lighttrack_core::new_id;

use super::fixtures::sample_entry;
use crate::{CollectiveFilter, Result, Store};

pub(super) fn collective(store: &dyn Store) -> Result<()> {
    let contributor = new_id();
    let mut e = sample_entry();
    e.contributor_id = contributor.clone();
    e.model = format!("conf-{}", new_id());
    store.upsert_collective_entry(&e)?;

    let mine = |store: &dyn Store| -> Result<Vec<_>> {
        Ok(store
            .list_collective_entries()?
            .into_iter()
            .filter(|x| x.contributor_id == contributor)
            .collect())
    };
    let listed = mine(store)?;
    assert_eq!(listed.len(), 1, "the entry is listed back");
    assert!(
        (listed[0].quality - e.quality).abs() < 1e-9,
        "quality round-trips"
    );
    assert_eq!(listed[0].n_cases, e.n_cases, "case count round-trips");

    // Re-sending the same key updates in place. Appending instead would let one contributor's repeat
    // submissions outvote everyone else's — the exact failure a k-anonymized digest exists to avoid.
    let mut again = e.clone();
    again.quality = 0.5;
    again.n_runs = 9;
    store.upsert_collective_entry(&again)?;
    let listed = mine(store)?;
    assert_eq!(listed.len(), 1, "upsert on the key, not append: {listed:?}");
    assert!(
        (listed[0].quality - 0.5).abs() < 1e-9,
        "the update won, not the original"
    );

    // A different task_type is a different key — the same contributor+model may hold several rows.
    let mut other_task = e.clone();
    other_task.task_type = "summarize".into();
    store.upsert_collective_entry(&other_task)?;
    assert_eq!(mine(store)?.len(), 2, "task_type is part of the key");

    // Retention purge is bounded by the cutoff: a fresh entry must survive an old-cutoff purge.
    store.purge_collective_entries_before(Utc::now() - Duration::days(365))?;
    assert_eq!(
        mine(store)?.len(),
        2,
        "a purge cutoff older than every entry removes none of them"
    );

    // --- the keyed read the ingest rate limit runs on every push ---
    let receipt = store
        .latest_collective_receipt(&contributor)?
        .expect("a contributor with entries has a receipt");
    let expected = mine(store)?
        .iter()
        .map(|x| x.received_at)
        .max()
        .expect("entries exist");
    assert_eq!(
        receipt, expected,
        "latest_collective_receipt is MAX(received_at) for that contributor, not for the table"
    );
    assert!(
        store.latest_collective_receipt(&new_id())?.is_none(),
        "an unknown contributor has no receipt — `None`, never another contributor's timestamp"
    );

    // --- the retention predicate the leaderboard pushes down ---
    let all = store.list_collective_entries_filtered(&CollectiveFilter::default())?;
    assert!(
        all.iter().any(|x| x.contributor_id == contributor),
        "an empty filter is the unfiltered list"
    );
    let future = store.list_collective_entries_filtered(&CollectiveFilter {
        received_after: Some(Utc::now() + Duration::days(1)),
    })?;
    assert!(
        !future.iter().any(|x| x.contributor_id == contributor),
        "received_after excludes entries received before it"
    );
    let past = store.list_collective_entries_filtered(&CollectiveFilter {
        received_after: Some(Utc::now() - Duration::days(365)),
    })?;
    assert_eq!(
        past.iter()
            .filter(|x| x.contributor_id == contributor)
            .count(),
        2,
        "a cutoff older than every entry keeps them all"
    );

    // --- replace: the contributor's set becomes exactly what was sent, in one call ---
    // The failure this guards is a *wrong* board, not a missing one: the old delete-then-N-upserts
    // loop could leave a bucket that has since fallen below the k-anonymity floor lingering beside
    // the new set, and that row would keep being published.
    let mut fresh = e.clone();
    fresh.task_type = "classify".into();
    fresh.quality = 0.77;
    let ack =
        store.replace_collective_contribution(&contributor, std::slice::from_ref(&fresh), None)?;
    assert_eq!(ack.deleted, 2, "replace removed the previous set: {ack:?}");
    assert_eq!(ack.inserted, 1, "replace wrote the new set: {ack:?}");
    assert_eq!(ack.purged, 0, "no cutoff given, so nothing was swept");
    let listed = mine(store)?;
    assert_eq!(
        listed.len(),
        1,
        "after a replace the contributor holds exactly the sent set — no survivors: {listed:?}"
    );
    assert_eq!(listed[0].task_type, "classify");
    assert!((listed[0].quality - 0.77).abs() < 1e-9);

    // A replace whose retention cutoff predates every entry sweeps nothing and, above all, does not
    // take the set it just wrote with it.
    let ack = store.replace_collective_contribution(
        &contributor,
        std::slice::from_ref(&fresh),
        Some(Utc::now() - Duration::days(365)),
    )?;
    assert_eq!(ack.purged, 0, "an old cutoff sweeps nothing: {ack:?}");
    assert_eq!(
        mine(store)?.len(),
        1,
        "the replaced set survives its own sweep"
    );

    // An empty replace is a withdrawal — the path a contributor whose every bucket fell under the
    // floor takes, and it must clear the set rather than silently keep publishing the old one.
    let ack = store.replace_collective_contribution(&contributor, &[], None)?;
    assert_eq!(ack.deleted, 1);
    assert_eq!(ack.inserted, 0);
    assert!(
        mine(store)?.is_empty(),
        "an empty replace leaves nothing behind"
    );

    // Withdrawal removes exactly this contributor's rows.
    store.upsert_collective_entry(&e)?;
    store.upsert_collective_entry(&other_task)?;
    let removed = store.delete_collective_entries(&contributor)?;
    assert_eq!(removed, 2, "delete reports the rows it removed");
    assert!(
        mine(store)?.is_empty(),
        "a contributor can withdraw everything they sent"
    );
    Ok(())
}
