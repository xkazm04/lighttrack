//! Incremental rolling-usage cache for admission control.
//!
//! [`events::usage_since`](super::events) re-aggregates the *whole* rolling window on every ingest —
//! a `SUM`/`COUNT` over every event in the window, once per distinct `(window, scope)` a rule uses,
//! all under the store's global connection lock. At a 30-day cap and steady traffic that is a
//! hundreds-of-thousands-of-row scan on *every* event; admission cost grows with window size.
//!
//! This cache makes per-ingest work `O(events inserted since the last check)`. Per
//! `(project, window, scope)` it keeps a running [`Usage`] total plus the still-in-window per-event
//! contributions, ordered by timestamp. Each read (1) pulls only events with a `rowid` past the last
//! one it folded in — a bounded range scan on the integer primary key — and adds them, then
//! (2) evicts the leading contributions that have aged out of the window. Both steps touch only the
//! delta, never the whole window.
//!
//! **Exactness.** The running totals reproduce `usage_since` exactly: `calls`/`tokens` are integer
//! sums (no drift), and `cost_usd` folds the same stored values `SUM` sees. Eviction — subtracting
//! events that leave the window — is the part a naive add-only cache gets wrong; it is covered
//! explicitly by the property tests in [`super::tests`], which assert the cache equals the full-scan
//! reference across randomized, boundary-straddling event sets and repeated window advances.
//!
//! **Atomicity.** The cache is updated under the *same* connection lock as the insert (see
//! `SqliteStore::insert_event_checked`), so the check-count-insert critical section is preserved: a
//! concurrent burst cannot read one stale total and race past a cap.
//!
//! **Monotonic `now`.** Reads assume a non-decreasing admission clock (wall-clock `Utc::now()` at
//! ingest, which only moves forward). Eviction is one-way — an event dropped at `now₁` stays dropped
//! at any `now₂ ≥ now₁`, matching the reference, which also excludes it. A clock that ran backwards
//! could under-count; ingest never does that.
//!
//! **Single-process assumption.** Loading by `rowid > seen` picks up *any* newly-committed event,
//! including one written by another process sharing the file, so appends never desync the totals.
//! What the cache cannot observe is another process's *admission decision* — cross-process ingest is
//! not serialized by this store's lock — so a configured cap is only strictly honored with one API
//! process per SQLite database (the repo's deployment stance).

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};

use lighttrack_core::{LimitScope, LimitWindow};

use crate::codec::fmt_ts;
use crate::{Result, Usage};

/// Cache key: one running total per project, rolling window, and optional dimension scope. A
/// project-wide cap and a scoped cap over the same window keep independent totals, exactly as the
/// admission evaluator reads them.
type Key = (String, LimitWindow, Option<LimitScope>);

/// Per-`SqliteStore` rolling-usage cache. Lives behind the store mutex and is mutated only inside the
/// admission critical section.
#[derive(Default)]
pub(super) struct UsageCache {
    buckets: HashMap<Key, Bucket>,
}

impl UsageCache {
    /// Current rolling [`Usage`] for `(project, window, scope)` as of `now`, folding in any events
    /// committed since the last call and evicting those that have aged out of the window.
    pub(super) fn usage(
        &mut self,
        conn: &Connection,
        project: &str,
        window: LimitWindow,
        scope: Option<&LimitScope>,
        now: DateTime<Utc>,
    ) -> Result<Usage> {
        let key = (project.to_string(), window, scope.cloned());
        let bucket = self.buckets.entry(key).or_default();
        bucket.load_new(conn, project, scope)?;
        bucket.evict_before(&fmt_ts(window.since(now)));
        Ok(bucket.total)
    }
}

/// Rolling state for one cache key.
#[derive(Default)]
struct Bucket {
    /// Running usage of the events currently inside the window.
    total: Usage,
    /// In-window per-event contributions keyed by `(ts, rowid)`: `ts` orders eviction chronologically
    /// (fixed-width RFC3339 strings compare correctly), `rowid` disambiguates equal timestamps.
    items: BTreeMap<(String, i64), Usage>,
    /// Highest `rowid` folded in so far; the next load pulls only `rowid > seen_rowid`, so an event
    /// is counted exactly once regardless of timestamp ties or out-of-order arrival.
    seen_rowid: i64,
}

impl Bucket {
    /// Fold in every stored event with a `rowid` past the last one seen. The query filters *only* on
    /// `rowid > seen`, which SQLite always serves as a range scan on the integer primary key (verified
    /// by an `EXPLAIN QUERY PLAN` test) — never a full window aggregate. The result is bounded by the
    /// events committed since the last check; project and scope are matched in Rust so the planner
    /// can't fall back to the `(project_id, ts)` index and scan the whole project instead.
    fn load_new(&mut self, conn: &Connection, project: &str, scope: Option<&LimitScope>) -> Result<()> {
        let mut stmt = conn.prepare(
            "SELECT rowid, project_id, provider, model, name, ts, cost_usd, \
             (input_tokens + output_tokens) FROM events WHERE rowid > ?1 ORDER BY rowid",
        )?;
        let rows = stmt
            .query_map(params![self.seen_rowid], NewRow::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for r in rows {
            // Advance past every scanned row — matching or not — so it is never re-scanned.
            if r.rowid > self.seen_rowid {
                self.seen_rowid = r.rowid;
            }
            if r.project != project {
                continue;
            }
            if let Some(s) = scope {
                if !s.matches(&r.provider, &r.model, r.name.as_deref()) {
                    continue;
                }
            }
            let contrib = Usage { cost_usd: r.cost.unwrap_or(0.0), calls: 1, tokens: r.tokens };
            self.total = self.total.plus(contrib);
            self.items.insert((r.ts, r.rowid), contrib);
        }
        Ok(())
    }

    /// Drop the leading contributions whose timestamp is before the window start, subtracting each
    /// from the running total. `BTreeMap` yields keys in `(ts, rowid)` order, so the front is always
    /// the oldest event.
    fn evict_before(&mut self, since: &str) {
        while let Some(key) = self.items.keys().next().cloned() {
            if key.0.as_str() < since {
                if let Some(contrib) = self.items.remove(&key) {
                    self.total = self.total.minus(contrib);
                }
            } else {
                break;
            }
        }
        // Snap to exact zero once the window is empty so repeated f64 add/subtract can't accumulate
        // drift across idle gaps — a fresh window then starts from a clean total.
        if self.items.is_empty() {
            self.total = Usage::default();
        }
    }
}

/// One freshly-loaded event row: enough to test project/scope membership and compute its usage
/// contribution.
struct NewRow {
    rowid: i64,
    project: String,
    provider: String,
    model: String,
    name: Option<String>,
    ts: String,
    cost: Option<f64>,
    tokens: i64,
}

impl NewRow {
    fn from_row(row: &Row) -> rusqlite::Result<NewRow> {
        Ok(NewRow {
            rowid: row.get(0)?,
            project: row.get(1)?,
            provider: row.get(2)?,
            model: row.get(3)?,
            name: row.get(4)?,
            ts: row.get(5)?,
            cost: row.get(6)?,
            tokens: row.get(7)?,
        })
    }
}
