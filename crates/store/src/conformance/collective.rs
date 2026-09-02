//! `Surface::Collective`: the opt-in shared leaderboard's entry table.
//!
//! The three properties the network effect rests on: an entry is **keyed** on
//! (contributor, provider, model, task_type) so a re-send updates rather than double-counts a
//! contributor's weight; a contributor can **withdraw** everything they sent; and the retention
//! purge is **time-bounded** — it must not take entries newer than the cutoff with it.

use chrono::{Duration, Utc};

use lighttrack_core::new_id;

use super::fixtures::sample_entry;
use crate::{Result, Store};

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

    // Withdrawal removes exactly this contributor's rows.
    let removed = store.delete_collective_entries(&contributor)?;
    assert_eq!(removed, 2, "delete reports the rows it removed");
    assert!(
        mine(store)?.is_empty(),
        "a contributor can withdraw everything they sent"
    );
    Ok(())
}
