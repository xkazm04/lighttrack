//! Admission control: the per-event and per-batch cap checks, and the concurrent-burst probe that
//! separates a genuinely atomic backend from one that caps on average.

use chrono::Utc;

use lighttrack_core::{new_id, LimitAction, LimitMetric, LimitRule, LimitWindow, LlmEvent};

use super::fixtures::sample_event;
use crate::{Admission, Result, Store};

pub(super) fn admission(store: &dyn Store) -> Result<()> {
    let pid = new_id();

    // No rules configured: every event is admitted and recorded.
    let first = store.insert_event_checked(&sample_event(&pid, "claude-haiku-4-5", 10, 5, 1.0))?;
    assert!(first.admitted, "no rules -> admitted");
    assert!(first.statuses.is_empty(), "no rules -> no statuses");

    // An Alert rule breaches but never blocks: the event is still recorded.
    let alert = LimitRule {
        id: new_id(),
        project_id: pid.clone(),
        metric: LimitMetric::Calls,
        window: LimitWindow::Hour,
        threshold: 1.0,
        action: LimitAction::Alert,
        enabled: true,
        warn_at: None,
        scope: None,
    };
    store.create_limit_rule(&alert)?;
    let alerted =
        store.insert_event_checked(&sample_event(&pid, "claude-haiku-4-5", 10, 5, 1.0))?;
    assert!(alerted.admitted, "Alert action never blocks ingest");
    assert!(
        alerted.statuses.iter().any(|s| s.breached),
        "Alert rule reports the breach"
    );

    // A Block rule on cost: usage is 2.0 so far; threshold 2.5. The next $1.0 event would push
    // usage-with-this-event to 3.0 >= 2.5, so it is rejected and not recorded.
    let block = LimitRule {
        id: new_id(),
        project_id: pid.clone(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Hour,
        threshold: 2.5,
        action: LimitAction::Block,
        enabled: true,
        warn_at: None,
        scope: None,
    };
    store.create_limit_rule(&block)?;
    let blocked =
        store.insert_event_checked(&sample_event(&pid, "claude-haiku-4-5", 10, 5, 1.0))?;
    assert!(!blocked.admitted, "Block rule rejects an over-cap event");
    assert!(
        blocked.statuses.iter().any(|s| s.rejects_ingest()),
        "rejection carries a breached enforcing status"
    );

    // The rejected event was never recorded: usage stays at the two admitted events.
    let u = store.usage_since(&pid, Utc::now() - chrono::Duration::hours(1))?;
    assert_eq!(u.calls, 2, "only the two admitted events are recorded");
    assert!(
        (u.cost_usd - 2.0).abs() < 1e-9,
        "rejected event's cost not counted"
    );
    Ok(())
}

/// Batch admission ([`Store::insert_events_checked`]): one result per item, in order; items already
/// accepted *earlier in the same batch* count toward the cap (so a caller can't bypass a limit by
/// packing events into one request); and a per-item store error lands in that item's slot instead of
/// poisoning the rest — the property a single-transaction port must not lose (on Postgres an
/// un-savepointed error aborts the whole transaction).
pub(super) fn admission_batch(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    store.create_limit_rule(&LimitRule {
        id: new_id(),
        project_id: pid.clone(),
        metric: LimitMetric::Calls,
        window: LimitWindow::Hour,
        threshold: 3.0,
        action: LimitAction::Block,
        enabled: true,
        warn_at: None,
        scope: None,
    })?;
    let batch: Vec<LlmEvent> = (0..5)
        .map(|_| sample_event(&pid, "claude-haiku-4-5", 1, 1, 0.0))
        .collect();
    let results = store.insert_events_checked(&batch);
    assert_eq!(results.len(), 5, "one result per batch item, in order");
    let mut admitted = 0;
    for r in results {
        if r?.admitted {
            admitted += 1;
        }
    }
    assert_eq!(
        admitted, 2,
        "in-batch accepted items count toward the cap of 3"
    );
    assert_eq!(
        store
            .usage_since(&pid, Utc::now() - chrono::Duration::hours(1))?
            .calls,
        2,
        "only the admitted items were persisted"
    );

    // Per-item failure isolation, on an uncapped project: a duplicate id in the middle must not cost
    // the items around it.
    let pid2 = new_id();
    let first = sample_event(&pid2, "claude-haiku-4-5", 1, 1, 0.0);
    let third = sample_event(&pid2, "claude-haiku-4-5", 1, 1, 0.0);
    let mixed = store.insert_events_checked(&[first.clone(), first.clone(), third]);
    assert!(
        matches!(mixed[0], Ok(ref a) if a.admitted),
        "first item admitted"
    );
    assert!(
        matches!(mixed[1], Err(crate::StoreError::Conflict(_))),
        "duplicate id is a typed per-item Conflict, got {:?}",
        mixed[1]
    );
    assert!(
        matches!(mixed[2], Ok(ref a) if a.admitted),
        "an item after a failed one still lands (the batch is not poisoned), got {:?}",
        mixed[2]
    );
    assert_eq!(
        store
            .usage_since(&pid2, Utc::now() - chrono::Duration::hours(1))?
            .calls,
        2,
        "the two distinct events are stored; the duplicate added nothing"
    );
    Ok(())
}

/// What a concurrent burst did to one cap: how many events the backend admitted, how many it
/// actually persisted, and the cap they were racing.
#[derive(Debug, Clone, Copy)]
pub struct RaceOutcome {
    /// The `calls` threshold the burst raced. An atomic backend admits at most `cap - 1` events (the
    /// event that would reach the threshold is the one rejected).
    pub cap: i64,
    pub admitted: i64,
    /// Events readable back from the store afterwards — must equal `admitted` (a rejected event is
    /// never recorded).
    pub stored: i64,
}

/// Fire `RACERS` simultaneous admissions at one fresh project guarded by a `Block` cap and report
/// what got through. Exposed (rather than inlined into [`run`]) so a caller can point it at a
/// *specific* admission path — the suite points it at [`Store::insert_event_checked`], and the
/// crate's own test points it at the trait's non-atomic default to prove this probe actually bites.
///
/// The barrier is the whole point: without it the calls trickle in and even a check-then-act
/// implementation looks correct, which is exactly how the cloud backends' advisory caps survived
/// review.
pub fn admission_race_probe(
    store: &dyn Store,
    admit: &(dyn Fn(&dyn Store, &LlmEvent) -> Result<Admission> + Sync),
) -> Result<RaceOutcome> {
    const RACERS: usize = 8;
    const CAP: i64 = 4;

    let pid = new_id();
    store.create_limit_rule(&LimitRule {
        id: new_id(),
        project_id: pid.clone(),
        metric: LimitMetric::Calls,
        window: LimitWindow::Hour,
        threshold: CAP as f64,
        action: LimitAction::Block,
        enabled: true,
        warn_at: None,
        scope: None,
    })?;

    let evs: Vec<LlmEvent> = (0..RACERS)
        .map(|_| sample_event(&pid, "claude-haiku-4-5", 1, 1, 0.0))
        .collect();
    let barrier = std::sync::Barrier::new(RACERS);
    let results: Vec<Result<Admission>> = std::thread::scope(|s| {
        let handles: Vec<_> = evs
            .iter()
            .map(|ev| {
                s.spawn(|| {
                    barrier.wait();
                    admit(store, ev)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("admission thread panicked"))
            .collect()
    });

    let mut admitted = 0;
    for r in results {
        if r?.admitted {
            admitted += 1;
        }
    }
    let stored = store
        .usage_since(&pid, Utc::now() - chrono::Duration::hours(1))?
        .calls;
    Ok(RaceOutcome {
        cap: CAP,
        admitted,
        stored,
    })
}

/// The cap must hold under a **simultaneous** burst, not merely under serial traffic — the property
/// `admission` above cannot see. A backend whose check-then-insert isn't one critical section lets
/// every racer read the same pre-burst usage and admit, so a cap of 4 quietly passes 8 events.
///
/// Backends that declare [`Store::admission_is_atomic`] `= false` are *reported*, not failed: an
/// honest advisory cap is a documented limitation (see the Firestore backend), while a backend
/// claiming atomicity and leaking is a correctness bug.
pub(super) fn admission_race(store: &dyn Store) -> Result<()> {
    let out = admission_race_probe(store, &|s, e| s.insert_event_checked(e))?;
    assert_eq!(
        out.stored, out.admitted,
        "every admitted event is recorded, and only those"
    );
    if store.admission_is_atomic() {
        assert!(
            out.admitted < out.cap,
            "atomic admission must keep a concurrent burst under the cap: {out:?}"
        );
    } else if out.admitted >= out.cap {
        eprintln!(
            "admission is advisory on this backend (admission_is_atomic() == false): a burst \
             admitted {} events against a cap of {}",
            out.admitted, out.cap
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{insert_event_checked_nonatomic, SqliteStore};

    /// A conformance check nobody has watched fail is a check nobody knows works. This pins that
    /// [`admission_race_probe`] distinguishes the two admission paths: SQLite's atomic override holds
    /// the cap, while the trait's non-atomic default — the one Postgres and Firestore inherited, over
    /// the *same* store — lets the burst through.
    #[test]
    fn race_probe_catches_the_non_atomic_admission_path() {
        let store = SqliteStore::open_in_memory().expect("in-memory store");
        for _ in 0..3 {
            let out = admission_race_probe(&store, &|s, e| s.insert_event_checked(e))
                .expect("atomic probe");
            assert!(
                out.admitted < out.cap,
                "atomic admission stays under the cap: {out:?}"
            );
        }
        // The default's usage read and insert are separate critical sections, so simultaneous racers
        // all count pre-burst usage. Sampled over a few rounds: the leak is a race, not a certainty.
        let leaked = (0..5).any(|_| {
            admission_race_probe(&store, &|s, e| insert_event_checked_nonatomic(s, e))
                .expect("non-atomic probe")
                .admitted
                >= 4
        });
        assert!(
            leaked,
            "the race probe must detect the non-atomic default over-admitting"
        );
    }
}
