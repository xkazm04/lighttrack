//! Types for the collective (shared-leaderboard) `Store` surface.
//!
//! The hub's ingest path is *replace a contributor's whole set*, not *append to it*: a bucket that
//! fell below the k-anonymity floor since the last push has to disappear, and a re-push must never
//! let one contributor's repeat submissions outvote everyone else's. Expressing that as a delete
//! followed by N upserts — each on its own connection — meant a failure mid-loop left a contributor
//! partially replaced, which is a **wrong** leaderboard rather than a missing one.
//!
//! [`ReplaceAck`] therefore reports what a replacement did *and whether it was one atomic unit*, so
//! the guarantee is data an operator (and the conformance suite) can read rather than a property
//! each backend quietly does or does not have.

use chrono::{DateTime, Utc};
use serde::Serialize;

use lighttrack_core::CollectiveEntry;

use crate::{Result, Store, StoreError};

/// What one [`Store::replace_collective_contribution`] actually did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ReplaceAck {
    /// Rows removed because they belonged to this contributor's previous set.
    pub deleted: u64,
    /// Rows written for the new set.
    pub inserted: u64,
    /// Rows the piggy-backed retention sweep removed (0 when no cutoff was given, or when the
    /// backend has no sweep — retention is also enforced at read time on every backend).
    pub purged: u64,
    /// Whether the delete and the inserts committed as **one** unit. `false` means a crash between
    /// them can leave a partially-replaced set; the caller may then prefer to re-push rather than
    /// trust the stored set. Declared, not assumed — see the module docs.
    pub atomic: bool,
}

/// Predicate for [`Store::list_collective_entries_filtered`].
///
/// Deliberately narrow: only predicates that are **safe before** the leaderboard's k-anonymity floor
/// belong here. Retention (`received_after`) is one — it drops rows that must not be published at
/// all. `provider` / `task_type` are **not**: pushing a user filter into the store would let it strip
/// a merged row down to a single contributor's private eval results. Those stay in memory, after the
/// merge and after the source floor (see `api/src/collective/leaderboard.rs`).
#[derive(Debug, Clone, Default)]
pub struct CollectiveFilter {
    /// Keep only entries received at or after this instant (the retention cutoff).
    pub received_after: Option<DateTime<Utc>>,
}

/// The composed, **non-atomic** fallback behind [`Store::replace_collective_contribution`]'s default.
///
/// Spelled out as a free function (like [`crate::insert_event_checked_nonatomic`]) so a backend that
/// cannot do better can call it explicitly and report `atomic: false`, rather than inheriting a
/// guarantee it does not have from a trait default nobody remembered was there.
pub fn replace_collective_contribution_nonatomic<S: Store + ?Sized>(
    store: &S,
    contributor_id: &str,
    entries: &[CollectiveEntry],
    purge_before: Option<DateTime<Utc>>,
) -> Result<ReplaceAck> {
    let deleted = store.delete_collective_entries(contributor_id)?;
    for e in entries {
        store.upsert_collective_entry(e)?;
    }
    Ok(ReplaceAck {
        deleted,
        inserted: entries.len() as u64,
        purged: sweep(store, purge_before)?,
        atomic: false,
    })
}

/// Run the retention sweep, tolerating a backend that has no sweep.
///
/// `Unsupported` is not an error here: the retention *policy* is enforced at read time on every
/// backend (the leaderboard drops expired entries before merging), so a missing sweep leaves dead
/// rows on disk but never publishes them.
pub(crate) fn sweep<S: Store + ?Sized>(store: &S, cutoff: Option<DateTime<Utc>>) -> Result<u64> {
    match cutoff {
        None => Ok(0),
        Some(c) => match store.purge_collective_entries_before(c) {
            Ok(n) => Ok(n),
            Err(StoreError::Unsupported(_)) => Ok(0),
            Err(e) => Err(e),
        },
    }
}

/// The composed default behind [`Store::latest_collective_receipt`]: scan and take the max.
pub(crate) fn latest_receipt_scanned<S: Store + ?Sized>(
    store: &S,
    contributor_id: &str,
) -> Result<Option<DateTime<Utc>>> {
    Ok(store
        .list_collective_entries()?
        .into_iter()
        .filter(|e| e.contributor_id == contributor_id)
        .map(|e| e.received_at)
        .max())
}

/// The composed default behind [`Store::list_collective_entries_filtered`]: read everything, then
/// apply the predicate in memory.
pub(crate) fn list_filtered_scanned<S: Store + ?Sized>(
    store: &S,
    f: &CollectiveFilter,
) -> Result<Vec<CollectiveEntry>> {
    let mut all = store.list_collective_entries()?;
    if let Some(after) = f.received_after {
        all.retain(|e| e.received_at >= after);
    }
    Ok(all)
}
