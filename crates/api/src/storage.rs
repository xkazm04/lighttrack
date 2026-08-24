//! The disk surface: what the store is costing, what it has measured about itself, and the
//! quiet-window maintenance that keeps the sidecar and the freelist from growing unattended.
//!
//! **Retention is deliberately unbounded (operator decision, 2026-08-24.)** Nothing in this module
//! deletes a row and nothing it calls can: `Store::maintenance_pass` takes no pruning parameter.
//! What it does is make the growth *visible* (`GET /v1/storage/status`) and *safe* (checkpoint the
//! journal, hand already-freed pages back to the filesystem) — see `docs/ARCHITECTURE.md` §12.
//!
//! ## The window is found, not scheduled
//!
//! A bare timer ("compact every N hours") is scheduling by wall clock, and wall clocks do not know
//! about requests: over enough runs the timer is guaranteed to fire mid-ingest, and the resulting
//! stall gets charged to whatever the user was doing. So every pass is gated on a live measurement
//! of whether this process is busy — [`ActivityGauge`], a count of in-flight requests incremented
//! and decremented at the router's own front door, read at the moment the pass would start and
//! **re-read between chunks while it runs**.
//!
//! The gate is two conditions, not one: the gauge reads zero AND a minimum interval has elapsed.
//! The interval bounds cost; the gauge bounds interference. The interval alone is the timer failure;
//! the gauge alone runs maintenance in every momentary gap between two requests, which turns idle
//! detection into a busy loop.
//!
//! ## Defer politely, but not forever
//!
//! A busy instance may present no perfect window for days while its journal grows, so deferral has
//! an escalation ladder ([`Rung`]): prefer true quiet; past a staleness bound accept *quieter* (a
//! reduced chunk while a little traffic is in flight); past a **hard bound stated as a harm** — the
//! journal exceeding N bytes, or the reclaimable share crossing a fraction of the file — run
//! regardless and say so in the record. The hard bound is deliberately expressed in bytes rather
//! than elapsed time: "the journal exceeds 64 MiB" is a reason a human can weigh, "it has been a
//! week" is the timer sneaking back in.
//!
//! ## Every pass is recorded, including the ones that did not happen
//!
//! Deferral is an *outcome*. "Ran", "ran and found nothing to do", "deferred because busy" and
//! "attempted and failed" are four different results, and a log that only records successes cannot
//! tell a healthy store from a scheduler that has been deferring for a month — a discovery that
//! otherwise arrives as a disk-full report. The flight recorder is a bounded in-memory ring served
//! by the same endpoint, which answers the two questions that otherwise become folklore: "is
//! maintenance actually running?" and "was that stall at 14:03 us?".

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::{Request, State},
    http::HeaderMap,
    middleware::Next,
    response::Response,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Serialize;

use lighttrack_store::{
    DbMetricsReport, MaintenanceOutcome, MaintenancePass, MaintenanceRequest, StorageReport,
};

use crate::error::{ApiError, ErrorCode};
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::{spawn_db, AppState};

/// Env: seconds between two evaluations of the maintenance gate. `0` disables the sweep entirely.
const ENV_SECS: &str = "LIGHTTRACK_MAINTENANCE_SECS";
/// Env: the minimum interval between two passes — the cost bound half of the two-condition gate.
const ENV_MIN_INTERVAL: &str = "LIGHTTRACK_MAINTENANCE_MIN_INTERVAL_SECS";
/// Env: how long the gate may keep deferring before it accepts "quieter" instead of "quiet".
const ENV_STALE_SECS: &str = "LIGHTTRACK_MAINTENANCE_STALE_SECS";
/// Env: journal bytes past which a pass runs regardless of activity. The harm, stated as a harm.
const ENV_WAL_HARD_BYTES: &str = "LIGHTTRACK_MAINTENANCE_WAL_HARD_BYTES";

/// How often the gate is *evaluated*. Evaluating is a cheap read; it is the pass that is gated.
const DEFAULT_SECS: u64 = 300;
/// Floor on the evaluation cadence, so a misconfiguration cannot turn the gate into a spin.
const MIN_SECS: u64 = 30;
/// Minimum spacing between passes.
const DEFAULT_MIN_INTERVAL_SECS: u64 = 900;
/// After this long with no pass, the ladder's second rung accepts a little activity.
const DEFAULT_STALE_SECS: u64 = 3_600;
/// The journal size that is itself the harm. 64 MiB of sidecar on a self-hosted box is worth one
/// brief writer pause; growing past it unattended is not.
const DEFAULT_WAL_HARD_BYTES: u64 = 64 * 1024 * 1024;
/// Reclaimable share of the file past which reclamation is worth doing on the escalated rung.
const RECLAIM_ESCALATE_SHARE: f64 = 0.25;
/// ...but never for a trivially small file: a quarter of 400 KiB is not worth a pass.
const RECLAIM_ESCALATE_FLOOR_BYTES: u64 = 16 * 1024 * 1024;
/// On the "quieter" rung, this much in-flight work is still acceptable.
const QUIETER_GAUGE_MAX: u64 = 1;
/// Chunks a single pass may run before yielding the loop back to the interval, so one pass can
/// never become an unbounded reclamation campaign.
const MAX_CHUNKS_PER_PASS: usize = 32;
const CHUNK_PAGES: u32 = 256;
const REDUCED_CHUNK_PAGES: u32 = 32;
/// Bounded flight recorder.
const RECORD_RING: usize = 32;
/// Journal bytes past which the pass upgrades from a passive checkpoint to a truncating one. Below
/// this the sidecar is doing its job (it is a cache of committed pages, not garbage) and cutting it
/// to zero every pass would only make the next writes re-grow it.
const WAL_SOFT_BYTES: u64 = 8 * 1024 * 1024;

/// A live count of in-flight foreground work, incremented and decremented at the router's front
/// door.
///
/// The gate must observe *actual demand for the machine*, not a proxy for it. Time-of-day is a proxy
/// (people work at night). "Idle since the last query", measured inside the store, is a proxy that
/// misses a request that is about to need the database in 200 ms. This counts requests that are
/// being handled right now, which is the thing itself.
///
/// It counts EVERY route, not only the ingest doors: a long analytical read holds a WAL snapshot and
/// is exactly the kind of foreground work a checkpoint should not compete with. (The ingest guard's
/// own in-flight number is a narrower thing — admission control for writes — and stays where it is.)
#[derive(Default)]
pub(crate) struct ActivityGauge {
    in_flight: AtomicU64,
    /// Highest concurrent depth seen, so a "gauge was 0" record can be read against what busy looks
    /// like on this instance.
    peak: AtomicU64,
    total: AtomicU64,
}

impl ActivityGauge {
    pub(crate) fn read(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Enter one unit of foreground work. The returned token decrements on drop, so a handler that
    /// panics, is cancelled, or returns early cannot leak a permanently-busy gauge — which would
    /// silently disable maintenance forever, the failure mode that is hardest to notice.
    fn enter(self: &Arc<Self>) -> ActivityToken {
        let depth = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(depth, Ordering::Relaxed);
        self.total.fetch_add(1, Ordering::Relaxed);
        ActivityToken(self.clone())
    }

    /// Test-only door onto [`ActivityGauge::enter`], which is otherwise reachable only through the
    /// middleware. Keeping the real one private is what guarantees there is exactly one place the
    /// gauge is incremented.
    #[cfg(test)]
    pub(crate) fn enter_for_test(self: &Arc<Self>) -> ActivityToken {
        self.enter()
    }
}

pub(crate) struct ActivityToken(Arc<ActivityGauge>);

impl Drop for ActivityToken {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Middleware: every request counts as foreground work while it is being handled.
pub(crate) async fn track_activity(
    State(st): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let _token = st.activity.enter();
    next.run(req).await
}

/// Which rung of the deferral ladder a pass ran on — or that it did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Rung {
    /// The gauge read zero and the interval had elapsed. The preferred case.
    Quiet,
    /// Deferral had gone on past the staleness bound, so a little activity was accepted and the
    /// chunk size reduced.
    Quieter,
    /// A stated harm crossed its bound (journal bytes, reclaimable share), so the pass ran anyway.
    Escalated,
    /// The gate said busy and nothing ran.
    Deferred,
}

/// One record in the flight recorder. Deferral is a first-class outcome here.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PassRecord {
    pub(crate) at: DateTime<Utc>,
    /// Why this evaluation did what it did, in words an operator can weigh.
    pub(crate) trigger: String,
    pub(crate) rung: Rung,
    /// The activity gauge at the moment the gate was evaluated.
    pub(crate) gauge: u64,
    /// `deferred` when nothing ran; otherwise the store's own verdict.
    pub(crate) outcome: String,
    pub(crate) duration_ms: u64,
    pub(crate) chunks: usize,
    pub(crate) pages_checkpointed: u64,
    pub(crate) pages_reclaimed: u64,
    /// Journal bytes before and after, so the record answers "did that help?".
    pub(crate) wal_bytes_before: Option<u64>,
    pub(crate) wal_bytes_after: Option<u64>,
    pub(crate) detail: String,
}

#[derive(Clone, Copy)]
pub(crate) struct SweepConfig {
    pub(crate) interval: Duration,
    pub(crate) min_interval: Duration,
    pub(crate) stale_after: Duration,
    pub(crate) wal_hard_bytes: u64,
}

impl SweepConfig {
    /// `None` when the sweep is switched off.
    ///
    /// On by default, unlike the forecast sweep next door — deliberately. That one turns a
    /// self-hosted process into an outbound notifier, which is a decision; this one keeps the
    /// process's own disk from growing unattended, which is upkeep. It is lossless, it is gated on
    /// idleness, and every pass it takes is recorded.
    pub(crate) fn from_env() -> Option<Self> {
        let secs = env_u64(ENV_SECS).unwrap_or(DEFAULT_SECS);
        if secs == 0 {
            return None;
        }
        Some(SweepConfig {
            interval: Duration::from_secs(secs.max(MIN_SECS)),
            min_interval: Duration::from_secs(
                env_u64(ENV_MIN_INTERVAL).unwrap_or(DEFAULT_MIN_INTERVAL_SECS),
            ),
            stale_after: Duration::from_secs(env_u64(ENV_STALE_SECS).unwrap_or(DEFAULT_STALE_SECS)),
            wal_hard_bytes: env_u64(ENV_WAL_HARD_BYTES).unwrap_or(DEFAULT_WAL_HARD_BYTES),
        })
    }
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|s| s.trim().parse().ok())
}

/// One line for the startup banner, so an operator can see at a glance whether anything will ever
/// checkpoint this database.
pub(crate) fn describe(cfg: Option<SweepConfig>) -> String {
    match cfg {
        None => format!("off (set {ENV_SECS})"),
        Some(c) => format!(
            "gate every {}s (min interval {}s, stale after {}s, journal hard bound {} MiB)",
            c.interval.as_secs(),
            c.min_interval.as_secs(),
            c.stale_after.as_secs(),
            c.wal_hard_bytes / (1024 * 1024),
        ),
    }
}

/// The sweep's own state: the flight recorder plus the counters an operator reads.
#[derive(Default)]
pub(crate) struct Maintenance {
    records: Mutex<Vec<PassRecord>>,
    ran: AtomicU64,
    nothing_to_do: AtomicU64,
    deferred: AtomicU64,
    failed: AtomicU64,
    pages_reclaimed: AtomicU64,
    pages_checkpointed: AtomicU64,
    /// `None` until the first pass actually runs — "never run" is its own state, not a zero.
    last_run: Mutex<Option<DateTime<Utc>>>,
}

impl Maintenance {
    fn record(&self, r: PassRecord) {
        match r.rung {
            Rung::Deferred => self.deferred.fetch_add(1, Ordering::Relaxed),
            _ => match r.outcome.as_str() {
                "ran" => {
                    *self.last_run.lock().unwrap() = Some(r.at);
                    self.ran.fetch_add(1, Ordering::Relaxed)
                }
                "failed" => self.failed.fetch_add(1, Ordering::Relaxed),
                _ => {
                    *self.last_run.lock().unwrap() = Some(r.at);
                    self.nothing_to_do.fetch_add(1, Ordering::Relaxed)
                }
            },
        };
        self.pages_reclaimed
            .fetch_add(r.pages_reclaimed, Ordering::Relaxed);
        self.pages_checkpointed
            .fetch_add(r.pages_checkpointed, Ordering::Relaxed);
        let mut ring = self.records.lock().unwrap();
        if ring.len() == RECORD_RING {
            ring.remove(0);
        }
        ring.push(r);
    }
}

/// Start the maintenance sweep as a detached task. No-op when it is switched off.
pub(crate) fn spawn(state: AppState, cfg: Option<SweepConfig>) {
    let Some(cfg) = cfg else { return };
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(cfg.interval);
        // The first tick fires immediately; skip it so a restart loop cannot turn into a pass loop.
        ticker.tick().await;
        let started = Instant::now();
        let mut last_pass: Option<Instant> = None;
        loop {
            ticker.tick().await;
            match evaluate(&state, &cfg, last_pass, started).await {
                Some(record) => {
                    if record.rung != Rung::Deferred {
                        last_pass = Some(Instant::now());
                    }
                    state.maintenance.record(record);
                }
                // The backend has no file to maintain (Postgres, Firestore). Stop the loop rather
                // than logging the same refusal every five minutes forever.
                None => {
                    tracing::info!(
                        "store maintenance sweep is not applicable to this backend; stopping the loop"
                    );
                    return;
                }
            }
        }
    });
}

/// Evaluate the gate once and, if it opens, run a chunked pass. `None` = this backend has no
/// storage to maintain.
async fn evaluate(
    state: &AppState,
    cfg: &SweepConfig,
    last_pass: Option<Instant>,
    started: Instant,
) -> Option<PassRecord> {
    let store = state.store.clone();
    let report = match spawn_db(move || store.storage_report()).await {
        Ok(r) => r,
        Err(e) if is_unsupported(&e) => return None,
        Err(e) => {
            return Some(PassRecord {
                at: Utc::now(),
                trigger: "could not read the storage report".into(),
                rung: Rung::Deferred,
                gauge: state.activity.read(),
                outcome: "failed".into(),
                duration_ms: 0,
                chunks: 0,
                pages_checkpointed: 0,
                pages_reclaimed: 0,
                wal_bytes_before: None,
                wal_bytes_after: None,
                detail: e.to_string(),
            })
        }
    };

    let gauge = state.activity.read();
    let since_pass = last_pass
        .map(|t| t.elapsed())
        .unwrap_or_else(|| started.elapsed());
    let wal = report.wal_bytes.unwrap_or(0);

    match decide(
        cfg,
        gauge,
        since_pass,
        wal,
        report.reclaimable_share,
        report.reclaimable_bytes,
    ) {
        Gate::Defer { trigger } => Some(deferred(gauge, trigger)),
        Gate::Run {
            rung,
            trigger,
            chunk,
        } => Some(run_chunked(state, report, rung, trigger, gauge, chunk, wal).await),
    }
}

/// What the gate decided, and why.
#[derive(Debug, PartialEq)]
pub(crate) enum Gate {
    Run {
        rung: Rung,
        trigger: String,
        chunk: u32,
    },
    Defer {
        trigger: String,
    },
}

/// The gate and the escalation ladder, as a pure function of the measurements.
///
/// Pulled out of the loop deliberately: this is the part with the interesting failure modes (a gate
/// that never opens, a gate that always opens, a hard bound quietly expressed as elapsed time), and
/// it is worth testing without a store, a runtime, or a clock.
pub(crate) fn decide(
    cfg: &SweepConfig,
    gauge: u64,
    since_pass: Duration,
    wal_bytes: u64,
    reclaimable_share: f64,
    reclaimable_bytes: u64,
) -> Gate {
    // Rung 3 first: a stated harm outranks politeness. Both bounds are expressed in BYTES — "the
    // journal exceeds 64 MiB" is a reason a human can weigh; "it has been a week" would be the wall
    // clock sneaking back in through the escalation door.
    if wal_bytes >= cfg.wal_hard_bytes {
        return Gate::Run {
            rung: Rung::Escalated,
            trigger: format!(
                "the journal is {} MiB, over the {} MiB hard bound — that size is the harm, so this                  pass runs regardless of the gauge",
                wal_bytes / (1024 * 1024),
                cfg.wal_hard_bytes / (1024 * 1024)
            ),
            chunk: CHUNK_PAGES,
        };
    }
    if reclaimable_share >= RECLAIM_ESCALATE_SHARE
        && reclaimable_bytes >= RECLAIM_ESCALATE_FLOOR_BYTES
    {
        return Gate::Run {
            rung: Rung::Escalated,
            trigger: format!(
                "{:.0}% of the file ({} MiB) is space the engine already freed and is not using —                  over the {:.0}% reclamation bound",
                reclaimable_share * 100.0,
                reclaimable_bytes / (1024 * 1024),
                RECLAIM_ESCALATE_SHARE * 100.0
            ),
            chunk: CHUNK_PAGES,
        };
    }
    // The cost half of the two-condition gate. Without it, an idle instance would run a pass every
    // evaluation — idle detection turned into a busy loop.
    if since_pass < cfg.min_interval {
        return Gate::Defer {
            trigger: format!(
                "only {}s since the last pass; the minimum interval is {}s (the cost bound half of                  the gate)",
                since_pass.as_secs(),
                cfg.min_interval.as_secs()
            ),
        };
    }
    // Rung 1: true quiet, the preferred case.
    if gauge == 0 {
        return Gate::Run {
            rung: Rung::Quiet,
            trigger: format!(
                "the gauge reads 0 and {}s have elapsed since the last pass",
                since_pass.as_secs()
            ),
            chunk: CHUNK_PAGES,
        };
    }
    // Rung 2: deferral has gone on long enough that "no maintenance" is the bigger risk, so accept a
    // little traffic and shrink the chunk.
    if since_pass >= cfg.stale_after && gauge <= QUIETER_GAUGE_MAX {
        return Gate::Run {
            rung: Rung::Quieter,
            trigger: format!(
                "no true quiet window for {}s (past the {}s staleness bound), so a reduced chunk                  runs against a gauge of {gauge}",
                since_pass.as_secs(),
                cfg.stale_after.as_secs()
            ),
            chunk: REDUCED_CHUNK_PAGES,
        };
    }
    Gate::Defer {
        trigger: format!(
            "{gauge} request(s) in flight; deferring (staleness bound {}s, {}s elapsed)",
            cfg.stale_after.as_secs(),
            since_pass.as_secs()
        ),
    }
}

fn deferred(gauge: u64, trigger: String) -> PassRecord {
    PassRecord {
        at: Utc::now(),
        trigger,
        rung: Rung::Deferred,
        gauge,
        outcome: "deferred".into(),
        duration_ms: 0,
        chunks: 0,
        pages_checkpointed: 0,
        pages_reclaimed: 0,
        wal_bytes_before: None,
        wal_bytes_after: None,
        detail: "nothing was attempted".into(),
    }
}

/// Run the pass as resumable chunks, re-reading the gauge between them.
///
/// The user does not stay away because maintenance started. Each chunk leaves the store consistent —
/// a pass abandoned halfway is merely incomplete, never corrupt — and the store's write lock is
/// released before the gauge is re-read, never held across it: the reverse order would keep the user
/// waiting on the very check meant to protect them.
async fn run_chunked(
    state: &AppState,
    report: StorageReport,
    rung: Rung,
    trigger: String,
    gauge: u64,
    chunk: u32,
    wal_before: u64,
) -> PassRecord {
    let t0 = Instant::now();
    let truncate = wal_before >= WAL_SOFT_BYTES;
    let mut chunks = 0usize;
    let mut checkpointed = 0u64;
    let mut reclaimed = 0u64;
    let mut detail = String::new();
    let mut outcome = MaintenanceOutcome::NothingToDo;

    for i in 0..MAX_CHUNKS_PER_PASS {
        let req = MaintenanceRequest {
            // Checkpoint once, on the first chunk; the rest are reclamation.
            truncate_wal: truncate && i == 0,
            reclaim_pages: chunk,
        };
        let store = state.store.clone();
        let pass: MaintenancePass = match spawn_db(move || store.maintenance_pass(req)).await {
            Ok(p) => p,
            Err(e) => {
                detail.push_str(&format!("chunk {i} failed: {e}. "));
                outcome = MaintenanceOutcome::Failed;
                break;
            }
        };
        chunks += 1;
        checkpointed += pass.pages_checkpointed;
        reclaimed += pass.pages_reclaimed;
        if pass.outcome == MaintenanceOutcome::Failed {
            outcome = MaintenanceOutcome::Failed;
            detail.push_str(&pass.detail);
            break;
        }
        if pass.pages_checkpointed > 0 || pass.pages_reclaimed > 0 {
            outcome = MaintenanceOutcome::Ran;
        }
        if pass.freelist_after == 0 {
            detail.push_str(&pass.detail);
            break;
        }
        // Yield, then RE-READ the gauge: the point of chunking is that a request arriving mid-pass
        // waits for one chunk, not for the whole reclamation. The escalated rung is the one case
        // that keeps going, because its trigger was a harm that is still true.
        tokio::task::yield_now().await;
        if rung != Rung::Escalated && state.activity.read() > 0 {
            detail.push_str(&format!(
                "yielded after {chunks} chunk(s): work arrived (gauge {}). ",
                state.activity.read()
            ));
            break;
        }
    }

    let store = state.store.clone();
    let after = spawn_db(move || store.storage_report()).await.ok();
    PassRecord {
        at: Utc::now(),
        trigger,
        rung,
        gauge,
        outcome: match outcome {
            MaintenanceOutcome::Ran => "ran",
            MaintenanceOutcome::NothingToDo => "nothing_to_do",
            MaintenanceOutcome::Failed => "failed",
        }
        .into(),
        duration_ms: t0.elapsed().as_millis() as u64,
        chunks,
        pages_checkpointed: checkpointed,
        pages_reclaimed: reclaimed,
        wal_bytes_before: report.wal_bytes,
        wal_bytes_after: after.and_then(|r| r.wal_bytes),
        detail: if detail.is_empty() {
            "completed".into()
        } else {
            detail.trim_end().into()
        },
    }
}

fn is_unsupported(e: &ApiError) -> bool {
    e.code() == ErrorCode::Unsupported
}

/// `GET /v1/storage/status` — the disk, the store's own latency, and the maintenance flight
/// recorder, in one operator surface.
///
/// **Admin-only.** It names the database path, every table's size and the process's internal
/// latency profile: an operational X-ray, not a tenant-scoped read.
pub(crate) async fn get_storage_status(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<StorageStatus>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;

    let store = st.store.clone();
    let storage = match spawn_db(move || store.storage_report()).await {
        Ok(r) => Some(r),
        Err(e) if is_unsupported(&e) => None,
        Err(e) => return Err(e),
    };
    let store = st.store.clone();
    let metrics = match spawn_db(move || store.db_metrics()).await {
        Ok(r) => Some(r),
        Err(e) if is_unsupported(&e) => None,
        Err(e) => return Err(e),
    };
    let unsupported = (storage.is_none()).then_some(
        "this store backend keeps no local file: disk accounting and maintenance belong to the \
         managed service that hosts it. The fields are null because nothing was measured — not \
         because nothing is there.",
    );

    let m = &st.maintenance;
    Ok(Json(StorageStatus {
        storage,
        db_metrics: metrics,
        unsupported,
        maintenance: MaintenanceStatus {
            sweep: st.maintenance_desc.clone(),
            last_run: *m.last_run.lock().unwrap(),
            passes_ran: m.ran.load(Ordering::Relaxed),
            passes_nothing_to_do: m.nothing_to_do.load(Ordering::Relaxed),
            passes_deferred: m.deferred.load(Ordering::Relaxed),
            passes_failed: m.failed.load(Ordering::Relaxed),
            pages_checkpointed_total: m.pages_checkpointed.load(Ordering::Relaxed),
            pages_reclaimed_total: m.pages_reclaimed.load(Ordering::Relaxed),
            activity_gauge: st.activity.read(),
            activity_peak: st.activity.peak.load(Ordering::Relaxed),
            recent: m.records.lock().unwrap().clone(),
            note: "Counters are process-local and reset on restart, the same honesty as \
                   /v1/ingest/status. `passes_deferred` is a first-class number: a store whose \
                   sweep has only ever deferred is indistinguishable from one with no sweep at all, \
                   and this is where that shows.",
        },
    }))
}

#[derive(Serialize)]
pub(crate) struct MaintenanceStatus {
    sweep: String,
    /// `null` until a pass has actually run — never-run is its own state, not a zero timestamp.
    last_run: Option<DateTime<Utc>>,
    passes_ran: u64,
    passes_nothing_to_do: u64,
    passes_deferred: u64,
    passes_failed: u64,
    pages_checkpointed_total: u64,
    pages_reclaimed_total: u64,
    activity_gauge: u64,
    activity_peak: u64,
    recent: Vec<PassRecord>,
    note: &'static str,
}

#[derive(Serialize)]
pub(crate) struct StorageStatus {
    storage: Option<StorageReport>,
    db_metrics: Option<DbMetricsReport>,
    /// Set when the backend has no local file, so a null `storage` reads as a refusal rather than
    /// an empty disk.
    unsupported: Option<&'static str>,
    maintenance: MaintenanceStatus,
}
