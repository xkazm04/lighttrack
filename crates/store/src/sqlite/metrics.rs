//! The store's own instrumentation: how long its operations take, keyed by operation family.
//!
//! No monitoring agent watches an embedded database. Until this module existed, the only timing code
//! in the whole store was an `#[ignore]`d hand-run harness (`bench.rs`), so "the app feels slow after
//! a few weeks" had nothing to interrogate — which is a bad look for a product whose entire job is
//! observing other people's latency. An observability service that cannot observe itself is quoting
//! folklore.
//!
//! ## What it keys by
//!
//! The **table or named operation family** — a closed vocabulary ([`DbOp`]) — never the statement
//! text. Statement text embeds values (unbounded cardinality) and shatters one logical hot path
//! across near-duplicate keys. Per-family keying is also what makes these numbers converse with
//! [`super::maintenance`]: the storage report says which table is *big*, this says which family is
//! *slow*, and "big AND degrading" is the strongest signal the pair produces.
//!
//! Waiting for a connection is its **own** key, never folded into query time — the two have disjoint
//! remedies (pool sizing / writer-hog vs. an index or a query rewrite), and a p95 that silently
//! mixes them indicts the wrong thing.
//!
//! ## "Slow" here is single-digit milliseconds
//!
//! Thresholds calibrated for networked databases are deaf to an embedded store: a local indexed read
//! that takes 50 ms is *pathological* — a missing index, a lock convoy, a checkpoint storm — while
//! sitting comfortably under any server-derived 100 ms line. Each family therefore declares its own
//! slow line ([`DbOp::slow`]), an order of magnitude above what that family should cost, and every
//! slow COUNT is reported with that predicate attached: "N operations over X ms on family F within
//! the current window" is a fact; "N slow queries" is a number two people will read differently.
//!
//! ## The instrument must not use the database
//!
//! Metrics that write to a metrics table turn every measured operation into two, contend for the
//! very locks being measured, and recurse the instrument into its own signal. Everything here is an
//! in-memory ring: constant-time record (atomics plus one short push into a fixed-size array), no
//! formatting on the fast path, and no lock shared with the measured path — the ring's mutex is per
//! family and is never held across a database call.
//!
//! ## Three consumers, each with a decision attached
//!
//! * the **warn channel** — a rate-limited `tracing::warn!` when an operation crosses its family's
//!   slow line. Suppression is itself counted: when a window rolls over having suppressed events,
//!   one summary line carries the suppressed count and the worst suppressed duration, because
//!   silent suppression turns "a burst happened" into "nothing happened" in exactly the moment the
//!   instrument exists for;
//! * the **maintenance gate** — [`super::maintenance`] passes are a family of their own, so
//!   "was that stall at 14:03 us?" is answerable;
//! * the **diagnostic surface** — `GET /v1/storage/status` renders [`crate::DbMetricsReport`], each
//!   figure naming how it was recomputed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::{DbMetricsReport, DbOpStats};

/// Recent durations kept per family, for percentile derivation. A ring, so memory is bounded and
/// old samples fall off — which is why every percentile below names the window it was computed over
/// rather than pretending to be all-time.
const RING: usize = 512;

/// Slow-operation reports allowed per family per [`WARN_WINDOW`] before suppression starts.
const WARN_BUDGET: u32 = 3;
/// The suppression window. On rollover, a window that suppressed anything says so.
const WARN_WINDOW: Duration = Duration::from_secs(60);

/// The closed vocabulary of measured operation families.
///
/// Adding a variant is a deliberate act (it is a new key an operator will read); adding a *statement*
/// is not. That asymmetry is the point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DbOp {
    EventsWrite,
    EventsRead,
    /// Rolling usage / cost aggregates — the reads that scan rather than seek, and the ones a cap
    /// check sits behind.
    UsageRead,
    ScoresWrite,
    ScoresRead,
    TracesRead,
    JobsWrite,
    JobsRead,
    /// Everything not named above, so no operation is invisible: an unkeyed family is a blind spot
    /// that reads as silence.
    OtherWrite,
    OtherRead,
    /// Checkpoint / reclamation passes. Their own family so a stall can be attributed to them.
    Maintenance,
    /// Waiting for a pooled read connection. Never folded into read time.
    PoolAcquire,
    /// Waiting for the single write connection. Contention, not engine work — a p95 driven by this
    /// indicts the writer-hog or the pool sizing, not the query plan.
    WriteLockWait,
}

impl DbOp {
    pub(crate) const ALL: [DbOp; 13] = [
        DbOp::EventsWrite,
        DbOp::EventsRead,
        DbOp::UsageRead,
        DbOp::ScoresWrite,
        DbOp::ScoresRead,
        DbOp::TracesRead,
        DbOp::JobsWrite,
        DbOp::JobsRead,
        DbOp::OtherWrite,
        DbOp::OtherRead,
        DbOp::Maintenance,
        DbOp::PoolAcquire,
        DbOp::WriteLockWait,
    ];

    /// The key an operator reads. Dotted `family.direction`, stable enough to grep for.
    pub(crate) fn key(self) -> &'static str {
        match self {
            DbOp::EventsWrite => "events.write",
            DbOp::EventsRead => "events.read",
            DbOp::UsageRead => "usage.read",
            DbOp::ScoresWrite => "scores.write",
            DbOp::ScoresRead => "scores.read",
            DbOp::TracesRead => "traces.read",
            DbOp::JobsWrite => "jobs.write",
            DbOp::JobsRead => "jobs.read",
            DbOp::OtherWrite => "other.write",
            DbOp::OtherRead => "other.read",
            DbOp::Maintenance => "maintenance",
            DbOp::PoolAcquire => "pool.acquire",
            DbOp::WriteLockWait => "write.lock.wait",
        }
    }

    /// This family's slow line, in milliseconds — an order of magnitude above what it should cost on
    /// a local file, not a threshold borrowed from a networked database.
    pub(crate) fn slow_ms(self) -> f64 {
        match self {
            // Point reads and single-row writes on an indexed local file: sub-millisecond healthy.
            DbOp::EventsRead
            | DbOp::ScoresRead
            | DbOp::JobsRead
            | DbOp::OtherRead
            | DbOp::ScoresWrite
            | DbOp::JobsWrite => 10.0,
            // Batch admission commits and fsyncs; a batch of 500 legitimately costs more.
            DbOp::EventsWrite | DbOp::OtherWrite => 50.0,
            // Aggregates scan a window; they earn a wider line, not an exemption.
            DbOp::UsageRead | DbOp::TracesRead => 50.0,
            // Waiting is never engine work: a wait this long means the pool is too small or someone
            // is holding the writer.
            DbOp::PoolAcquire => 5.0,
            DbOp::WriteLockWait => 100.0,
            // A pass is allowed to take a while; it is supposed to run when nobody is waiting.
            DbOp::Maintenance => 2_000.0,
        }
    }

    /// Whether rows-written is a meaningful figure for this family. A read never changes a row, and
    /// reporting `0` for one would read as "the write did nothing" rather than "this is not a write".
    fn counts_rows(self) -> bool {
        matches!(
            self,
            DbOp::EventsWrite
                | DbOp::ScoresWrite
                | DbOp::JobsWrite
                | DbOp::OtherWrite
                | DbOp::Maintenance
        )
    }
}

#[derive(Default)]
struct Ring {
    /// Microseconds. `u32` caps at ~71 minutes, which no store operation reaches without the process
    /// already being the incident.
    samples: Vec<u32>,
    next: usize,
}

impl Ring {
    fn push(&mut self, us: u32) {
        if self.samples.len() < RING {
            self.samples.push(us);
        } else {
            self.samples[self.next] = us;
            self.next = (self.next + 1) % RING;
        }
    }
}

struct WarnBudget {
    window_started: Instant,
    emitted: u32,
    suppressed: u32,
    worst_suppressed_us: u32,
}

impl Default for WarnBudget {
    fn default() -> Self {
        Self {
            window_started: Instant::now(),
            emitted: 0,
            suppressed: 0,
            worst_suppressed_us: 0,
        }
    }
}

/// What the rate limiter decided about one breach. Extracted from the emit path so the policy — the
/// part with the interesting failure mode — is testable without standing up a log subscriber.
#[derive(Debug, PartialEq, Eq)]
struct Decision {
    /// Report this breach.
    emit: bool,
    /// A window just rolled over having suppressed `(count, worst_us)`. `None` when the window that
    /// closed suppressed nothing — "found nothing" and "did not look" must not read alike, and an
    /// unconditional summary line saying "suppressed 0" would train the reader to skip it.
    summary: Option<(u32, u32)>,
}

fn decide(b: &mut WarnBudget, us: u32) -> Decision {
    let mut summary = None;
    if b.window_started.elapsed() >= WARN_WINDOW {
        if b.suppressed > 0 {
            summary = Some((b.suppressed, b.worst_suppressed_us));
        }
        *b = WarnBudget::default();
    }
    if b.emitted < WARN_BUDGET {
        b.emitted += 1;
        Decision {
            emit: true,
            summary,
        }
    } else {
        b.suppressed += 1;
        b.worst_suppressed_us = b.worst_suppressed_us.max(us);
        Decision {
            emit: false,
            summary,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Silent suppression converts "a burst happened" into "nothing happened", which is the
    /// instrument lying in exactly the moment it exists for.
    #[test]
    fn suppression_is_counted_and_the_worst_one_survives_the_window() {
        let mut b = WarnBudget::default();
        for i in 0..WARN_BUDGET {
            assert_eq!(
                decide(&mut b, 1_000 + i),
                Decision {
                    emit: true,
                    summary: None
                },
                "the first {WARN_BUDGET} breaches in a window are reported"
            );
        }
        // Past the budget: counted, not emitted, and the worst is remembered.
        assert_eq!(
            decide(&mut b, 9_000),
            Decision {
                emit: false,
                summary: None
            }
        );
        assert!(!decide(&mut b, 40_000).emit);
        assert!(!decide(&mut b, 2_000).emit);
        assert_eq!(b.suppressed, 3);
        assert_eq!(b.worst_suppressed_us, 40_000, "the worst, not the last");

        // Roll the window over: the closing window reports what it hid.
        b.window_started = Instant::now() - WARN_WINDOW - Duration::from_secs(1);
        let d = decide(&mut b, 1_500);
        assert_eq!(d.summary, Some((3, 40_000)));
        assert!(d.emit, "and the new window starts with its budget restored");
    }

    /// A window that suppressed nothing says nothing — otherwise every window emits a line reading
    /// "suppressed 0" and the reader is trained to skip exactly the line that matters.
    #[test]
    fn a_quiet_window_produces_no_summary() {
        let mut b = WarnBudget::default();
        assert!(decide(&mut b, 1_000).emit);
        b.window_started = Instant::now() - WARN_WINDOW - Duration::from_secs(1);
        assert_eq!(decide(&mut b, 1_000).summary, None);
    }

    /// "Slow" is a per-family claim. One borrowed number would be deaf to the difference between a
    /// point read and a 500-item batch commit — and a networked database's 100 ms line would call
    /// a pathological 50 ms local read healthy.
    #[test]
    fn each_family_declares_its_own_slow_line() {
        assert!(DbOp::EventsRead.slow_ms() < DbOp::EventsWrite.slow_ms());
        assert!(DbOp::PoolAcquire.slow_ms() < DbOp::EventsRead.slow_ms());
        assert!(DbOp::Maintenance.slow_ms() > DbOp::UsageRead.slow_ms());
        for op in DbOp::ALL {
            assert!(op.slow_ms() > 0.0, "{} has no slow line", op.key());
            assert!(
                op.slow_ms() <= 2_000.0,
                "{} borrowed a networked-database threshold",
                op.key()
            );
        }
    }

    /// The vocabulary is closed and every key is distinct — a duplicated key would silently merge
    /// two families' numbers into one row nobody could act on.
    #[test]
    fn the_vocabulary_is_closed_and_unique() {
        let mut keys: Vec<&str> = DbOp::ALL.iter().map(|o| o.key()).collect();
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n);
        assert!(keys
            .iter()
            .all(|k| !k.contains("SELECT") && !k.contains('?')));
    }
}

#[derive(Default)]
struct KeyStats {
    count: AtomicU64,
    total_us: AtomicU64,
    max_us: AtomicU64,
    slow: AtomicU64,
    rows: AtomicU64,
    ring: Mutex<Ring>,
    warn: Mutex<WarnBudget>,
}

/// The in-memory instrument. One per store.
pub(crate) struct Meter {
    keys: Vec<KeyStats>,
    started: Instant,
}

impl Default for Meter {
    fn default() -> Self {
        Self {
            keys: (0..DbOp::ALL.len()).map(|_| KeyStats::default()).collect(),
            started: Instant::now(),
        }
    }
}

impl Meter {
    fn slot(&self, op: DbOp) -> &KeyStats {
        let i = DbOp::ALL.iter().position(|o| *o == op).unwrap_or(0);
        &self.keys[i]
    }

    /// Record one operation. Constant time on the fast path: five relaxed atomic adds and one push
    /// into a fixed-size array. Nothing is formatted unless the operation was already slow.
    pub(crate) fn record(&self, op: DbOp, took: Duration, rows: Option<u64>) {
        let us = took.as_micros().min(u32::MAX as u128) as u32;
        let k = self.slot(op);
        k.count.fetch_add(1, Ordering::Relaxed);
        k.total_us.fetch_add(us as u64, Ordering::Relaxed);
        k.max_us.fetch_max(us as u64, Ordering::Relaxed);
        if let Some(r) = rows {
            k.rows.fetch_add(r, Ordering::Relaxed);
        }
        if let Ok(mut ring) = k.ring.lock() {
            ring.push(us);
        }
        let ms = us as f64 / 1000.0;
        if ms >= op.slow_ms() {
            k.slow.fetch_add(1, Ordering::Relaxed);
            self.warn_rate_limited(op, k, us);
        }
    }

    /// The push-mode consumer. Every breach carries key, duration and the threshold it crossed, so
    /// the line is actionable without a dashboard — and suppression is itself reported, because a
    /// retry storm's hundredth slow query is noise but the fact that there were a hundred is the
    /// finding.
    fn warn_rate_limited(&self, op: DbOp, k: &KeyStats, us: u32) {
        let Ok(mut b) = k.warn.lock() else { return };
        let d = decide(&mut b, us);
        if let Some((suppressed, worst_us)) = d.summary {
            tracing::warn!(
                db_op = op.key(),
                suppressed,
                worst_suppressed_ms = worst_us as f64 / 1000.0,
                window_secs = WARN_WINDOW.as_secs(),
                slow_over_ms = op.slow_ms(),
                "slow store operations were suppressed by the report budget"
            );
        }
        if d.emit {
            tracing::warn!(
                db_op = op.key(),
                took_ms = us as f64 / 1000.0,
                slow_over_ms = op.slow_ms(),
                "slow store operation"
            );
        }
    }

    /// The pull-mode consumer: derive the report from the rings at read time.
    pub(crate) fn report(&self) -> DbMetricsReport {
        let mut ops = Vec::new();
        for op in DbOp::ALL {
            let k = self.slot(op);
            let count = k.count.load(Ordering::Relaxed);
            if count == 0 {
                // A family nobody called is omitted rather than rendered as a row of zeros: a zero
                // p95 for a path that never ran is a number someone will quote.
                continue;
            }
            let mut samples: Vec<u32> =
                k.ring.lock().map(|r| r.samples.clone()).unwrap_or_default();
            samples.sort_unstable();
            let pct = |p: usize| -> f64 {
                if samples.is_empty() {
                    return 0.0;
                }
                let i = (samples.len() * p / 100).min(samples.len() - 1);
                samples[i] as f64 / 1000.0
            };
            let total_us = k.total_us.load(Ordering::Relaxed);
            ops.push(DbOpStats {
                key: op.key(),
                count,
                mean_ms: total_us as f64 / count as f64 / 1000.0,
                p50_ms: pct(50),
                p95_ms: pct(95),
                max_ms: k.max_us.load(Ordering::Relaxed) as f64 / 1000.0,
                sampled: samples.len(),
                slow_count: k.slow.load(Ordering::Relaxed),
                slow_over_ms: op.slow_ms(),
                rows_written: op.counts_rows().then(|| k.rows.load(Ordering::Relaxed)),
            });
        }
        DbMetricsReport {
            since_secs: self.started.elapsed().as_secs(),
            ring_capacity: RING,
            ops,
            recomputation: "count / mean / max / slow_count are cumulative since process start \
                 (they reset on restart, like every other counter this product exposes). p50 and p95 \
                 are recomputed at read time from the last `sampled` durations for that key — a ring \
                 of at most `ring_capacity`, so they describe recent behaviour, not the whole run, \
                 and a quiet key's percentiles may be hours old. slow_count is 'operations at or over \
                 that key's own slow_over_ms since process start' — it is not comparable across keys, \
                 whose thresholds differ. rows_written is null for read families because a read \
                 changes no rows, which is a different statement from changing zero.",
            note: "In-process and in-memory, by design: an instrument that wrote to the database \
                 would double every measured operation and contend for the locks it is measuring.",
        }
    }
}
