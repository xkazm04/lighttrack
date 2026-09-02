//! Atomic admission control on Postgres: evaluate the project's caps and persist the event as **one
//! transaction**, so a configured cap actually caps under concurrent ingest.
//!
//! **Why a per-project transaction-scoped advisory lock.** Admission is check-then-act: read rolling
//! usage, decide, insert. Wrapping the three reads/writes in a plain transaction is *not* enough —
//! under READ COMMITTED two concurrent transactions both read pre-burst usage and both insert, and
//! under SERIALIZABLE they'd instead abort each other, turning a traffic burst into a retry storm on
//! the ingest path (and every retry re-reads the whole window). `pg_advisory_xact_lock` taken as the
//! transaction's *first* statement makes admission for one project a genuine critical section: a
//! second admission for the same project blocks until the first commits, then reads usage that
//! already includes it. The lock is released by the commit/rollback — there is no leak path if the
//! connection dies mid-transaction.
//!
//! Contention behavior: admissions for a *single* project serialize (one commit per event, the same
//! shape as the SQLite backend's connection lock); different projects never block each other, since
//! the lock key is derived from the project id. Waiters queue on the lock rather than spinning, so
//! there is no livelock — the cost of a burst is latency, not lost enforcement. Nothing else in the
//! codebase takes advisory locks, and a batch takes its locks in sorted project order, so the lock
//! order is total and deadlock-free.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::{Connection, PgConnection, Postgres, Transaction};

use lighttrack_core::{scope_matches, LimitRule, LimitScope, LimitWindow, LlmEvent};
use lighttrack_store::{
    evaluate_admission, event_contribution, Admission, Result, StoreError, Usage,
};

use crate::events::{insert_err, insert_query, map_usage, RECEIVED, USAGE_COLS};
use crate::projects::{limit_rule_from_row, LIMIT_COLS};
use crate::util::{fmt_ts, pgerr};

/// Serialize admission for one project. Taken as the first statement of the transaction and held
/// until it ends; `hashtextextended` gives a deterministic 64-bit key from the project id.
async fn lock_project(conn: &mut PgConnection, project: &str) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(project.to_string())
        .execute(&mut *conn)
        .await
        .map_err(pgerr)?;
    Ok(())
}

async fn rules_in_tx(conn: &mut PgConnection, project: &str) -> Result<Vec<LimitRule>> {
    let sql = format!("SELECT {LIMIT_COLS} FROM limit_rules WHERE project_id = $1 AND enabled = 1");
    let rows = sqlx::query(&sql)
        .bind(project.to_string())
        .fetch_all(&mut *conn)
        .await
        .map_err(pgerr)?;
    rows.iter().map(limit_rule_from_row).collect()
}

/// Rolling usage inside the admission transaction — so it sees this transaction's own prior inserts
/// (the batch path's "previously-accepted items count toward the cap") and nobody else's uncommitted
/// ones. Windowed on `received_at`, never the client `ts`.
async fn usage_in_tx(
    conn: &mut PgConnection,
    project: &str,
    since: chrono::DateTime<Utc>,
    scope: Option<&LimitScope>,
) -> Result<Usage> {
    let row = match scope {
        None => {
            let sql = format!(
                "SELECT {USAGE_COLS} FROM events WHERE project_id = $1 AND {RECEIVED} >= $2"
            );
            sqlx::query(&sql)
                .bind(project.to_string())
                .bind(fmt_ts(since))
                .fetch_one(&mut *conn)
                .await
        }
        Some(s) => {
            // A fixed literal chosen by the enum discriminant (never user input) — safe to
            // interpolate. Shared with the pooled read path via `scope_expr` so a new scope
            // dimension cannot be taught to one and not the other: `api_key` and `customer` are
            // metadata extractions rather than columns.
            let expr = crate::events::scope_expr(s.kind_str()).unwrap_or("NULL");
            let sql = format!(
                "SELECT {USAGE_COLS} FROM events \
                 WHERE project_id = $1 AND {RECEIVED} >= $2 AND {expr} = $3"
            );
            sqlx::query(&sql)
                .bind(project.to_string())
                .bind(fmt_ts(since))
                .bind(s.value().to_string())
                .fetch_one(&mut *conn)
                .await
        }
    }
    .map_err(pgerr)?;
    map_usage(&row)
}

/// Evaluate `rules` against this event and insert it if admitted — all on `conn`, which the caller
/// has already locked for the project.
///
/// `evaluate_admission` (the shared evaluator that decides scope matching, imputation and shedding)
/// takes a *synchronous* usage lookup, so the distinct `(window, scope)` totals the applicable rules
/// need are fetched first and handed to it from a map. The pre-pass applies the same
/// `scope_matches` predicate the evaluator does, so it never fetches usage for a rule that cannot
/// apply, and never misses one that can.
async fn admit_one(
    conn: &mut PgConnection,
    ev: &LlmEvent,
    rules: &[LimitRule],
) -> Result<Admission> {
    let now = Utc::now();
    let mut usages: HashMap<(LimitWindow, Option<LimitScope>), Usage> = HashMap::new();
    for r in rules {
        if !scope_matches(r.scope.as_ref(), &ev.scope_dims()) {
            continue;
        }
        let key = (r.window, r.scope.clone());
        if usages.contains_key(&key) {
            continue;
        }
        let u = usage_in_tx(
            &mut *conn,
            &ev.project_id,
            r.window.since(now),
            r.scope.as_ref(),
        )
        .await?;
        usages.insert(key, u);
    }
    // Revenue-share thresholds are resolved inside the same advisory-locked transaction as the
    // usage reads and the insert, so the cap and the revenue it derives from are one snapshot. The
    // helper short-circuits when no rule needs revenue, so a fixed-cap deployment pays nothing.
    let resolved = resolve_revenue_thresholds(&mut *conn, ev, rules, now).await?;
    let resolve = lighttrack_store::resolver(&resolved);
    let admission = evaluate_admission(
        rules,
        ev,
        event_contribution(ev),
        |w, scope| {
            usages.get(&(w, scope.cloned())).copied().ok_or_else(|| {
                StoreError::Other(
                    "admission: usage for an applicable rule was not prefetched".into(),
                )
            })
        },
        resolve,
    )?;
    if admission.admitted {
        insert_query(ev)?
            .execute(&mut *conn)
            .await
            .map_err(|e| insert_err(e, &ev.id))?;
    }
    Ok(admission)
}

/// Resolve every revenue-share rule's threshold on `conn`. The revenue rows are read here (once per
/// distinct window, inside the same advisory-locked transaction as the usage reads and the insert),
/// then handed to the shared pure resolver — so Postgres and SQLite compute "80% of revenue" with
/// one implementation rather than two that could drift.
async fn resolve_revenue_thresholds(
    conn: &mut PgConnection,
    ev: &LlmEvent,
    rules: &[LimitRule],
    now: DateTime<Utc>,
) -> Result<HashMap<String, (f64, lighttrack_core::ThresholdBasis)>> {
    if !rules.iter().any(lighttrack_store::needs_revenue) {
        return Ok(HashMap::new());
    }
    let mut windows: lighttrack_store::RevenueWindows = HashMap::new();
    for r in rules.iter().filter(|r| lighttrack_store::needs_revenue(r)) {
        let key = lighttrack_store::window_key(r);
        if let std::collections::hash_map::Entry::Vacant(slot) = windows.entry(key) {
            let rows =
                crate::revenue::list_in_tx(&mut *conn, &ev.project_id, r.window.since(now), now)
                    .await?;
            slot.insert(rows);
        }
    }
    Ok(lighttrack_store::resolve_from_windows(rules, now, &windows))
}

pub(crate) async fn insert_event_checked(pool: &PgPool, ev: &LlmEvent) -> Result<Admission> {
    let mut tx: Transaction<'_, Postgres> = pool.begin().await.map_err(pgerr)?;
    lock_project(&mut tx, &ev.project_id).await?;
    let rules = rules_in_tx(&mut tx, &ev.project_id).await?;
    let out = admit_one(&mut tx, ev, &rules).await;
    match out {
        Ok(a) => {
            tx.commit().await.map_err(pgerr)?;
            Ok(a)
        }
        Err(e) => {
            // Rollback is best-effort: the error we return is the one that matters, and the advisory
            // lock is released either way when the transaction ends.
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

/// Batch admission: **one** transaction for the whole batch, matching the SQLite semantics.
///
/// Each item runs inside its own SAVEPOINT, because Postgres aborts the entire transaction on any
/// statement error — without one, a single duplicate id (23505) would poison every following item
/// and turn a per-item `Conflict` into a lost batch. Rolling back the savepoint leaves the prior
/// accepted items intact and *counted*: the next item's usage read runs in the same transaction and
/// therefore sees them, so a caller cannot bypass a cap by packing events into one request.
pub(crate) async fn insert_events_checked(
    pool: &PgPool,
    evs: &[LlmEvent],
) -> Vec<Result<Admission>> {
    if evs.is_empty() {
        return Vec::new();
    }
    let all = |e: StoreError| -> Vec<Result<Admission>> {
        let msg = e.to_string();
        evs.iter()
            .map(|_| Err(StoreError::Other(msg.clone())))
            .collect()
    };
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(e) => return all(pgerr(e)),
    };
    // Distinct projects, sorted: a total lock order, so two batches touching the same pair of
    // projects can never deadlock against each other. (A batch is single-project by construction
    // today; this keeps that from being a load-bearing assumption.)
    let mut projects: Vec<&str> = evs.iter().map(|e| e.project_id.as_str()).collect();
    projects.sort_unstable();
    projects.dedup();
    for p in &projects {
        if let Err(e) = lock_project(&mut tx, p).await {
            let _ = tx.rollback().await;
            return all(e);
        }
    }

    let mut rules_by_project: HashMap<String, Vec<LimitRule>> = HashMap::new();
    let mut out: Vec<Result<Admission>> = Vec::with_capacity(evs.len());
    for ev in evs {
        if !rules_by_project.contains_key(&ev.project_id) {
            match rules_in_tx(&mut tx, &ev.project_id).await {
                Ok(r) => {
                    rules_by_project.insert(ev.project_id.clone(), r);
                }
                Err(e) => {
                    out.push(Err(e));
                    continue;
                }
            }
        }
        let rules = &rules_by_project[&ev.project_id];
        let mut sp = match tx.begin().await {
            Ok(sp) => sp,
            Err(e) => {
                out.push(Err(pgerr(e)));
                continue;
            }
        };
        match admit_one(&mut sp, ev, rules).await {
            Ok(a) => match sp.commit().await {
                Ok(()) => out.push(Ok(a)),
                Err(e) => out.push(Err(pgerr(e))),
            },
            Err(e) => {
                let _ = sp.rollback().await;
                out.push(Err(e));
            }
        }
    }
    if let Err(e) = tx.commit().await {
        // All-or-nothing beats a torn batch the client can't detect.
        return all(pgerr(e));
    }
    out
}
