//! LightTrack persistence layer.
//!
//! [`Store`] is the backend-agnostic interface used by `api` (and later `mcp`/`cli`). The local
//! implementation is [`sqlite::SqliteStore`]; cloud backends slot in behind the same trait, selected
//! by `LIGHTTRACK_DATABASE_URL`: `lighttrack-store-pg` (Postgres, the cross-cloud default) and
//! `lighttrack-store-firestore` (GCP-native). See `docs/PACKAGING.md`.
//!
//! Methods are synchronous (SQLite is blocking). Async callers wrap them in `spawn_blocking`.

pub mod codec;
pub mod conformance;
pub mod sqlite;

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use lighttrack_core::{
    scope_matches, ApiKey, Benchmark, BenchmarkRun, CollectiveEntry, CostByDimension, CostEvidence,
    Dataset, DatasetItem, Job, JobCancel, JobFinish, LimitMetric, LimitRule, LimitScope,
    LimitStatus, LimitWindow, LlmEvent, ModelPriceRow, Project, Prompt, PromptVersion,
    RelayOutcome, RelayTask, RevenueEvent, Rubric, Score, TokensByDimension, Trace, TraceSummary,
};

pub use sqlite::SqliteStore;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A write violated a uniqueness/primary-key constraint (e.g. a duplicate event `id`). Distinct
    /// from `Other` so the API can map it to a 409 Conflict instead of an opaque 500. Backends that
    /// don't classify constraint violations simply never produce it (their duplicate writes surface
    /// as `Sqlite`/`Other`, i.e. current behavior) — SQLite detects and raises it.
    #[error("conflict: {0}")]
    Conflict(String),
    /// The backend has not ported this capability. Distinct from `Other` so the API can answer 501
    /// (`unsupported`) instead of an opaque 500 — and so a permanent capability gap is never
    /// mistaken for a transient outage (or, worse, for "no data": trait defaults used to return
    /// empty results here, which read as authoritative zeros on unported backends). The payload
    /// names the capability; the full message is stable enough to log but not to parse.
    #[error("{0} is not supported by this store backend")]
    Unsupported(&'static str),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A cost/usage rollup row (grouped by project + provider + model).
#[derive(Debug, Clone, Serialize)]
pub struct CostRow {
    pub project_id: String,
    pub provider: String,
    pub model: String,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
}

/// Optional filters + keyset cursor for [`Store::list_events_filtered`]. All fields are additive and
/// AND-combined; `None` fields don't constrain. `cursor` is an opaque token minted by a previous page
/// ([`EventPage::next_cursor`]) — the backend decodes it into a `(ts, id)` keyset position.
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub trace_id: Option<String>,
    pub name: Option<String>,
    /// Call outcome: `success` | `error` | `timeout`. The first question asked when debugging.
    pub status: Option<String>,
    /// Match events carrying this tag (membership in the `tags` array, not a substring).
    pub tag: Option<String>,
    /// Match events whose `metadata` has this key. Combined with [`EventFilter::metadata_value`] it
    /// becomes an equality test — which is how you ask "everything for this customer", since the
    /// billing linkage rides in metadata rather than a column.
    pub metadata_key: Option<String>,
    pub metadata_value: Option<String>,
    /// Minimum resolved `cost_usd` (inclusive). Trace listing already had this; the flat event list
    /// did not, so "which individual calls are expensive" had no answer.
    pub min_cost: Option<f64>,
    /// Also compute the total number of matching events (ignoring the cursor and page limit), so a
    /// client can render "n of N" without paging the whole result set to count it. Opt-in: it costs a
    /// second aggregate query, which a plain "give me the latest 50" should not pay for.
    pub with_total: bool,
    pub cursor: Option<String>,
}

impl EventFilter {
    /// The first predicate set here that a backend without the extended event-query support can't
    /// honor, or `None` when this filter only uses the original fields.
    ///
    /// Backends call this and return [`StoreError::Unsupported`] → HTTP 501 instead of quietly
    /// ignoring the predicate. Silently ignoring it is the dangerous option: an operator asking
    /// "show me the errored calls" would get a page of successful ones and believe it, and a
    /// filter that returns *more* than asked reads as authoritative rather than broken.
    pub fn unsupported_extension(&self) -> Option<&'static str> {
        if self.status.is_some() {
            return Some("the `status` event filter");
        }
        if self.tag.is_some() {
            return Some("the `tag` event filter");
        }
        if self.metadata_key.is_some() {
            return Some("the metadata event filter");
        }
        if self.min_cost.is_some() {
            return Some("the `min_cost` event filter");
        }
        if self.with_total {
            return Some("the event total count");
        }
        None
    }
}

/// One page of events plus the cursor to fetch the next page (newest-first). `next_cursor` is `Some`
/// only when more rows exist beyond this page; pass it back as [`EventFilter::cursor`] to continue.
#[derive(Debug, Clone)]
pub struct EventPage {
    pub events: Vec<LlmEvent>,
    pub next_cursor: Option<String>,
    /// Total matching events, ignoring the cursor and page limit — `Some` only when the filter asked
    /// for it ([`EventFilter::with_total`]).
    pub total: Option<u64>,
}

/// Optional filters + keyset cursor for [`Store::list_traces_filtered`]. All fields AND-combine;
/// `None` fields don't constrain. `since`/`until` bound the trace's `ended` (its newest event); of
/// these `since` is pushed to the event scan (index-served) while `until`, `status`, and `min_cost`
/// are aggregate-level (applied after grouping). `cursor` is an opaque token minted by a previous
/// page ([`TracePage::next_cursor`]) that the backend decodes into an `(ended, trace_id)` keyset
/// position. `status` is `"success"` or `"error"` (a trace is `error` iff any span errored).
#[derive(Debug, Clone, Default)]
pub struct TraceFilter {
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub status: Option<String>,
    pub min_cost: Option<f64>,
    pub cursor: Option<String>,
}

/// One page of trace summaries plus the cursor to fetch the next page (newest-ended first).
/// `next_cursor` is `Some` only when more traces remain beyond this page; pass it back as
/// [`TraceFilter::cursor`] to continue.
#[derive(Debug, Clone)]
pub struct TracePage {
    pub traces: Vec<TraceSummary>,
    pub next_cursor: Option<String>,
}

/// A bounded window of one trace's events, plus how many it really has.
///
/// The detail path fetches at most [`MAX_TRACE_SPANS`] spans; `total` is the trace's true span count,
/// so a clipped read is reported as clipped ([`Trace::spans_truncated`]) instead of passing for a
/// whole trace. `total == events.len()` on the untruncated (normal) case.
#[derive(Debug, Clone)]
pub struct TraceEvents {
    pub events: Vec<LlmEvent>,
    pub total: usize,
}

/// Ceiling on how many spans one trace-detail read materializes. A runaway agent loop can put an
/// unbounded number of spans behind a single `trace_id`, and the detail path — unlike the paginated
/// listing — had no cap, so every fetch and every whole-trace scoring cycle grew with it.
pub const MAX_TRACE_SPANS: usize = 5_000;

/// One cost/usage bucket in a single customer's margin breakdown — grouped by model (`provider/model`)
/// or by use-case `name`. `key` is that bucket label (`unattributed` / `(unnamed)` for the null group).
#[derive(Debug, Clone, Serialize)]
pub struct CustomerCostRow {
    pub key: String,
    pub calls: i64,
    pub cost_usd: f64,
}

/// A use-case cost/usage rollup row — grouped by (name, provider, model), optionally windowed by a
/// `since` cutoff. `name` is `None` for calls that carry no use-case name; the consumer rolls those
/// up under their model. Powers the Personas "LLM Overview" table.
#[derive(Debug, Clone, Serialize)]
pub struct UseCaseCostRow {
    pub name: Option<String>,
    pub provider: String,
    pub model: String,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
}

/// One UTC calendar day's aggregated usage for a project — a point in the dense daily series that
/// trend forecasting fits. `day` is the `YYYY-MM-DD` prefix of the (fixed-width, UTC) event `ts`.
/// Days with no traffic are simply absent; the caller densifies the gaps to zero.
#[derive(Debug, Clone, Serialize)]
pub struct DailyUsage {
    pub day: String,
    pub cost_usd: f64,
    pub calls: i64,
    pub tokens: i64,
}

/// One UTC day's aggregated LLM cost for a single billing-dimension value (customer/product), for
/// margin-trend forecasting. `key` is `None` for untagged (unattributed) cost.
#[derive(Debug, Clone, Serialize)]
pub struct DailyDimCost {
    pub day: String,
    pub key: Option<String>,
    pub cost_usd: f64,
    pub calls: i64,
}

/// Aggregate usage for a project over a time window — used to evaluate limits.
///
/// `cost_usd` is the **stored** sum and stays exactly that: what a `SUM(cost_usd)` sees, with
/// unpriced events contributing nothing. `unpriced_calls` and `client_cost_usd` qualify it, and the
/// limit path reads [`Usage::cost_for_limits`] (stored + imputed) rather than the bare sum.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Usage {
    pub cost_usd: f64,
    pub calls: i64,
    pub tokens: i64,
    /// Calls in this snapshot whose model was absent from the price book, so `cost_usd` is `NULL` on
    /// the row and they add `$0.00` to `cost_usd`. Counted (never priced) on the event itself; the
    /// limit path charges them via imputation instead.
    #[serde(default)]
    pub unpriced_calls: i64,
    /// The part of `cost_usd` that came from a client-supplied number (`metadata.cost_source =
    /// "client"`) rather than our own price-book arithmetic.
    #[serde(default)]
    pub client_cost_usd: f64,
}

impl Usage {
    /// The value of `metric` in this snapshot, as the comparable `f64` limits evaluate against. For
    /// `cost_usd` this is [`Usage::cost_for_limits`] — the stored sum **plus** the imputed cost of
    /// unpriced traffic — so a model missing from the price book can't be spent for free.
    pub fn metric_value(&self, metric: LimitMetric) -> f64 {
        match metric {
            LimitMetric::CostUsd => self.cost_for_limits(),
            LimitMetric::Calls => self.calls as f64,
            LimitMetric::Tokens => self.tokens as f64,
        }
    }

    /// Provenance of this snapshot's cost: priced vs unpriced calls, the imputed charge for the
    /// unpriced ones, the client-self-reported share, and whether the window is unpriceable.
    ///
    /// **Imputation rule:** each unpriced call is charged the mean cost of a *priced* call in the
    /// same window (`cost_usd / priced_calls`). It uses only evidence already inside the window —
    /// no provider lookups, no writes to the event row — and it self-corrects: as an operator adds
    /// the missing price, newly-priced traffic moves the mean the estimate is drawn from. With no
    /// priced call in the window there is nothing to learn from, and the snapshot is *unpriceable*.
    pub fn cost_evidence(&self) -> CostEvidence {
        let priced = self.calls - self.unpriced_calls;
        let imputed = if priced > 0 && self.unpriced_calls > 0 {
            (self.cost_usd / priced as f64) * self.unpriced_calls as f64
        } else {
            0.0
        };
        CostEvidence {
            priced_calls: priced.max(0),
            unpriced_calls: self.unpriced_calls,
            imputed_cost_usd: imputed,
            client_reported_cost_usd: self.client_cost_usd,
            unpriceable: self.unpriced_calls > 0 && priced <= 0,
        }
    }

    /// The cost figure a `cost_usd` limit is evaluated against: stored cost plus the imputed charge
    /// for unpriced traffic.
    pub fn cost_for_limits(&self) -> f64 {
        self.cost_usd + self.cost_evidence().imputed_cost_usd
    }

    /// Sum two usage snapshots (e.g. rolling usage plus one candidate event's contribution).
    pub fn plus(self, other: Usage) -> Usage {
        Usage {
            cost_usd: self.cost_usd + other.cost_usd,
            calls: self.calls + other.calls,
            tokens: self.tokens + other.tokens,
            unpriced_calls: self.unpriced_calls + other.unpriced_calls,
            client_cost_usd: self.client_cost_usd + other.client_cost_usd,
        }
    }

    /// Subtract one snapshot from another — the inverse of [`Usage::plus`], used to remove an event's
    /// contribution from a running rolling total when it ages out of the window (see the SQLite
    /// backend's `usage_cache`).
    pub fn minus(self, other: Usage) -> Usage {
        Usage {
            cost_usd: self.cost_usd - other.cost_usd,
            calls: self.calls - other.calls,
            tokens: self.tokens - other.tokens,
            unpriced_calls: self.unpriced_calls - other.unpriced_calls,
            client_cost_usd: self.client_cost_usd - other.client_cost_usd,
        }
    }
}

/// Rolling usage for one *value* of a scope dimension — one API key, one customer, one model…
///
/// This is the pre-breach view of a scoped cap: it answers "how much has each key spent so far",
/// which is exactly the question `/v1/limits/status` could not answer until a rule already existed
/// and had already tripped.
#[derive(Debug, Clone, Serialize)]
pub struct ScopeUsage {
    /// The dimension value, or `None` for traffic that carries no value on this dimension (unnamed
    /// calls, untagged customers, events written before key attribution existed).
    pub value: Option<String>,
    #[serde(flatten)]
    pub usage: Usage,
}

/// Evaluate one rule against a usage snapshot — the single place a [`LimitStatus`] is built from a
/// [`Usage`], shared by ingest admission (`evaluate_admission`) and the read-only
/// `/v1/limits/status` surface so the two can never disagree about what a cap currently says.
///
/// `cost_usd` rules carry their [`CostEvidence`]; `calls`/`tokens` rules don't (there is nothing to
/// qualify — a call is a call).
pub fn evaluate_rule(rule: &LimitRule, usage: &Usage) -> LimitStatus {
    let evidence = matches!(rule.metric, LimitMetric::CostUsd).then(|| usage.cost_evidence());
    rule.evaluate_with_evidence(usage.metric_value(rule.metric), evidence)
}

/// Outcome of an admission-controlled ingest ([`Store::insert_event_checked`]).
#[derive(Debug, Clone)]
pub struct Admission {
    /// Whether the event was persisted. `false` means a limit turned it away — either a hard stop
    /// (a breached `Throttle`/`Block` rule, or an unpriceable cost cap) or a graduated `Throttle`
    /// shed. The API surfaces both as HTTP 429; [`Admission::shed`] tells them apart.
    pub admitted: bool,
    /// Limit statuses evaluated against rolling usage *including* the candidate event. On a shed,
    /// the rule(s) that shed this event carry `shedding = true`.
    pub statuses: Vec<LimitStatus>,
    /// `true` when the rejection was graduated back-pressure (a `Throttle` rule shedding below its
    /// threshold) rather than a hard stop. Nothing is over budget yet: the same event may be
    /// accepted on a later attempt with a fresh id, and other traffic is still flowing.
    pub shed: bool,
    /// How long the client should wait before retrying, when rejected. Short for a shed, window-
    /// scaled for a hard stop. `None` when admitted.
    pub retry_after_secs: Option<u64>,
}

impl Admission {
    /// Decide the outcome from the evaluated statuses. Rejects on a hard stop
    /// ([`LimitStatus::rejects_ingest`]) or a graduated shed, and derives the retry hint from
    /// whichever rejection is in force — a hard stop outranks a shed, since it is the longer wait.
    pub(crate) fn from_statuses(statuses: Vec<LimitStatus>) -> Self {
        let hard = statuses.iter().any(|s| s.rejects_ingest());
        let shed = !hard && statuses.iter().any(|s| s.shedding);
        let admitted = !hard && !shed;
        let retry_after_secs = statuses
            .iter()
            .filter(|s| s.rejects_ingest())
            .map(|s| s.retry_after_secs())
            .max()
            .or_else(|| {
                statuses
                    .iter()
                    .filter(|s| s.shedding)
                    .map(|s| s.retry_after_secs())
                    .max()
            });
        Admission {
            admitted,
            statuses,
            shed,
            retry_after_secs,
        }
    }
}

/// One event's contribution to rolling usage: one call, its cost, and its prompt+completion tokens
/// (matching `usage_since`, which sums `input + output`).
///
/// An event with no resolved cost still contributes `$0.00` of *stored* cost — the price book had no
/// entry and we refuse to invent one on the row — but it is now counted in `unpriced_calls`, so the
/// limit path can charge it by imputation instead of letting it ride for free.
pub fn event_contribution(ev: &LlmEvent) -> Usage {
    let cost = ev.cost_usd.unwrap_or(0.0);
    Usage {
        cost_usd: cost,
        calls: 1,
        tokens: (ev.usage.input + ev.usage.output) as i64,
        unpriced_calls: i64::from(ev.cost_usd.is_none()),
        client_cost_usd: if ev.cost_is_client_reported() {
            cost
        } else {
            0.0
        },
    }
}

/// Evaluate `rules` against rolling usage that *includes* `contribution` (the candidate event `ev`),
/// looking up each distinct `(window, scope)`'s current usage via `current_usage`. Shared by the
/// trait's default (non-atomic) admission path and backends' transactional overrides so they agree
/// on semantics.
///
/// **Scope semantics:** a scoped rule only *applies* to an event whose dimensions match its scope —
/// non-matching scoped rules are skipped entirely (never evaluated, never able to reject this
/// event), so a "cap gpt-4o" rule can't turn away a gpt-4o-mini call. Every rule that *does* apply
/// (unscoped, or scoped-and-matching) folds the candidate into its own `(window, scope)` usage, then
/// breaches when that usage reaches its threshold; the event is admitted only if no applied enforcing
/// rule breaches.
///
/// **Unpriced traffic:** a `cost_usd` rule evaluates against [`Usage::cost_for_limits`], which charges
/// calls the price book couldn't price at the window's own mean priced cost. When the window has *no*
/// priced call at all the cap is unpriceable and an enforcing rule rejects — see [`CostEvidence`].
pub fn evaluate_admission<F>(
    rules: &[LimitRule],
    ev: &LlmEvent,
    contribution: Usage,
    mut current_usage: F,
) -> Result<Admission>
where
    F: FnMut(LimitWindow, Option<&LimitScope>) -> Result<Usage>,
{
    let dims = ev.scope_dims();
    // Usage cache now keys by (window, scope): a scoped cap and a project-wide cap over the same
    // window read different rolling totals.
    let mut prospective: HashMap<(LimitWindow, Option<LimitScope>), Usage> = HashMap::new();
    let mut statuses = Vec::new();
    for r in rules {
        if !scope_matches(r.scope.as_ref(), &dims) {
            continue; // a scoped rule the candidate doesn't match can neither count it nor reject it
        }
        let key = (r.window, r.scope.clone());
        let usage = match prospective.get(&key) {
            Some(u) => *u,
            None => {
                // Applied rule → the candidate matches this scope → fold it into the scoped total.
                let u = current_usage(r.window, r.scope.as_ref())?.plus(contribution);
                prospective.insert(key, u);
                u
            }
        };
        let mut st = evaluate_rule(r, &usage);
        // Graduated throttling is decided here, where the candidate event is known: a `Throttle`
        // rule past its ramp start sheds a proportional, deterministic share of traffic. Recorded on
        // the status so the rejection ledger and the alerts attribute the shed to the right rule.
        st.shedding = st.sheds(&ev.id);
        statuses.push(st);
    }
    Ok(Admission::from_statuses(statuses))
}

/// **Non-atomic** admission: `list_limit_rules` → `usage_since` → `insert_event` as three separate
/// store calls with no lock spanning them.
///
/// This is a last resort for a backend that cannot express a check-then-insert as one critical
/// section, and it is *named* rather than hidden inside a trait default so nobody mistakes it for an
/// implementation: between the usage read and the insert, any concurrent ingest can insert too, so a
/// burst can all read the same pre-burst usage and sail past the cap (check-then-act TOCTOU). A
/// backend using it must report [`Store::admission_is_atomic`] `= false` and say so at startup — an
/// advisory cap that reads as enforced is the bug; an honest refusal is not.
pub fn insert_event_checked_nonatomic<S: Store + ?Sized>(
    store: &S,
    ev: &LlmEvent,
) -> Result<Admission> {
    let rules = store.list_limit_rules(&ev.project_id, true)?;
    let now = Utc::now();
    let admission =
        evaluate_admission(&rules, ev, event_contribution(ev), |w, scope| match scope {
            None => store.usage_since(&ev.project_id, w.since(now)),
            Some(s) => store.usage_since_scoped(&ev.project_id, w.since(now), s),
        })?;
    if admission.admitted {
        store.insert_event(ev)?;
    }
    Ok(admission)
}

/// Batch form of [`insert_event_checked_nonatomic`]: one independent (and individually non-atomic)
/// admission per item. Cap-honest *within* the batch only because each accepted insert is committed
/// before the next item's usage read.
pub fn insert_events_checked_nonatomic<S: Store + ?Sized>(
    store: &S,
    evs: &[LlmEvent],
) -> Vec<Result<Admission>> {
    evs.iter()
        .map(|e| insert_event_checked_nonatomic(store, e))
        .collect()
}

/// How a byte figure in a [`StorageReport`] was obtained.
///
/// The two answers are *different claims*, and conflating them makes the report unable to answer its
/// own follow-up question ("will anything shrink the file?"): pages allocated to an object include
/// the free space inside those pages; live bytes do not. They diverge by exactly the reclaimable
/// space, which is the number the maintenance pass acts on.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ByteMeasure {
    /// Summed `dbstat.pgsize` — **bytes of pages allocated** to the object, free space within those
    /// pages included. This is what the engine's own page accounting reports.
    PagesAllocated,
    /// The engine could not be asked (the `dbstat` virtual table is not compiled into this SQLite),
    /// so per-object bytes are `None`. Deliberately not a zero: "I could not look" and "there is
    /// nothing there" are different findings, and a zero would read as the second.
    Unavailable,
}

impl ByteMeasure {
    /// The predicate every byte figure in the report travels with.
    pub fn predicate(self) -> &'static str {
        match self {
            ByteMeasure::PagesAllocated => {
                "bytes of pages allocated to the object (dbstat.pgsize), free space inside those \
                 pages included — not bytes of live rows"
            }
            ByteMeasure::Unavailable => {
                "not measured: this SQLite build has no dbstat virtual table, so per-object bytes \
                 are unknown (not zero)"
            }
        }
    }
}

/// One accounted object in the store — a table or one of its indexes.
///
/// Indexes are listed as their own rows rather than folded into their table: an index that has
/// outgrown its table is a different finding, with a different remedy, from a table that has grown.
#[derive(Debug, Clone, Serialize)]
pub struct StorageObject {
    pub name: String,
    /// `table` or `index`.
    pub kind: String,
    /// `None` for indexes (an index has no independent row count) and whenever counting failed.
    pub rows: Option<i64>,
    /// `None` when [`StorageReport::measured`] is [`ByteMeasure::Unavailable`].
    pub bytes: Option<u64>,
    /// This object's share of `db_bytes`, `None` when bytes are unmeasured.
    pub share: Option<f64>,
}

/// Per-object disk accounting for an embedded store, plus what a maintenance pass could reclaim.
///
/// The unit of actionability is the object, not the file: "the database is 2 GB" triggers panic,
/// "one table is 1.7 GB of it" triggers a fix. Every byte figure carries how it was measured
/// ([`ByteMeasure`]), and the file-level figures name their own source in their doc comments.
#[derive(Debug, Clone, Serialize)]
pub struct StorageReport {
    /// Which backend answered — a report that does not say what it measured is a rumour.
    pub backend: &'static str,
    /// The database file, when the backend has one.
    pub path: Option<String>,
    /// `PRAGMA page_size`.
    pub page_size: u64,
    /// `PRAGMA page_count × page_size` — the main database file's size as the engine accounts it.
    pub db_bytes: u64,
    /// The write-ahead journal sidecar, from the filesystem. `None` when there is no file to stat
    /// (in-memory) or the stat failed — never a zero, which would read as "the WAL is empty".
    pub wal_bytes: Option<u64>,
    /// `PRAGMA freelist_count × page_size` — pages the engine already owns and will reuse, which a
    /// reclamation pass can return to the filesystem **without deleting a single row**.
    pub reclaimable_bytes: u64,
    /// `reclaimable_bytes / db_bytes`, the ratio the reclamation trigger reads.
    pub reclaimable_share: f64,
    /// `none` | `full` | `incremental` — decides whether reclamation can happen in yieldable chunks
    /// (`incremental`) or only as a full offline rewrite (`none`).
    pub auto_vacuum: &'static str,
    /// Whether `maintenance_pass` can reclaim on this file at all, and what to do when it cannot.
    pub reclaim_note: String,
    pub measured: ByteMeasure,
    /// The predicate for every `bytes` figure below, carried in the payload so a number quoted out
    /// of this report keeps its meaning.
    pub bytes_predicate: &'static str,
    /// Largest first.
    pub objects: Vec<StorageObject>,
    /// What this store deletes on its own, stated where the disk is measured. Retention is a product
    /// decision, and the current one is written here rather than left to be inferred from an empty
    /// list of pruners.
    pub retention: &'static str,
}

/// What one maintenance pass was asked to do. Both actions are **lossless**: a checkpoint moves
/// already-committed pages from the journal into the database, and incremental vacuum returns pages
/// the engine had already freed. Neither deletes a row, and there is no pruning door here on purpose.
#[derive(Debug, Clone, Copy)]
pub struct MaintenanceRequest {
    /// Truncate the WAL back to zero bytes rather than the routine passive checkpoint. The heavier
    /// form: it needs the writer briefly and is reserved for the escalation rung where the sidecar
    /// itself is the stated harm.
    pub truncate_wal: bool,
    /// Pages of already-freed space to return to the filesystem this pass. `0` skips reclamation.
    /// This is the chunk size — the caller re-reads its activity gauge between chunks.
    pub reclaim_pages: u32,
}

/// How a maintenance pass ended. Three outcomes, never two: a log that cannot distinguish "ran and
/// found nothing to do" from "attempted and failed" cannot tell a healthy store from a broken
/// scheduler, and the difference arrives later as a disk-full report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintenanceOutcome {
    /// Work was done — pages checkpointed and/or reclaimed.
    Ran,
    /// The pass executed and there was nothing to do (empty journal, no free pages).
    NothingToDo,
    /// The engine refused or errored; `detail` carries what it said.
    Failed,
}

/// One record in the store's maintenance flight recorder.
#[derive(Debug, Clone, Serialize)]
pub struct MaintenancePass {
    pub outcome: MaintenanceOutcome,
    pub duration_ms: u64,
    /// Pages moved from the journal into the database file.
    pub pages_checkpointed: u64,
    /// Pages returned to the filesystem by incremental vacuum.
    pub pages_reclaimed: u64,
    /// `PRAGMA freelist_count` before and after, so "we reclaimed nothing" is distinguishable from
    /// "there was nothing to reclaim".
    pub freelist_before: u64,
    pub freelist_after: u64,
    /// What the engine said, or what was skipped and why.
    pub detail: String,
}

/// One measured operation family's numbers.
///
/// Every figure here travels with what it means; see [`DbMetricsReport::recomputation`], which is
/// served in the same payload rather than living in a doc the reader of a number will not have open.
#[derive(Debug, Clone, Serialize)]
pub struct DbOpStats {
    /// The family key — a table or a named operation family, never statement text.
    pub key: &'static str,
    /// Operations recorded since process start.
    pub count: u64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
    /// How many recent durations the percentiles above were computed from.
    pub sampled: usize,
    /// Operations at or over `slow_over_ms`. The count is meaningless without the threshold, so the
    /// threshold ships beside it in every row.
    pub slow_count: u64,
    pub slow_over_ms: f64,
    /// `None` for read families: a read changes no rows, which is a different statement from
    /// changing zero rows.
    pub rows_written: Option<u64>,
}

/// The store's view of its own behaviour.
#[derive(Debug, Clone, Serialize)]
pub struct DbMetricsReport {
    pub since_secs: u64,
    /// Per-key ring size — the bound on how much history the percentiles can see.
    pub ring_capacity: usize,
    /// Only families that have actually run. A family nobody called is omitted rather than rendered
    /// as a row of zeros, which is a number someone would quote.
    pub ops: Vec<DbOpStats>,
    /// How each figure above was derived, in the payload.
    pub recomputation: &'static str,
    pub note: &'static str,
}

/// Backend-agnostic persistence interface.
pub trait Store: Send + Sync {
    /// Create tables if they don't exist.
    fn init_schema(&self) -> Result<()>;

    /// Persist one normalized event.
    fn insert_event(&self, ev: &LlmEvent) -> Result<()>;

    /// Admission-controlled ingest: evaluate the project's enabled limit rules against rolling
    /// usage *including this event* and persist the event only if no enforcing (`Throttle`/`Block`)
    /// rule would be breached. Returns whether the event was admitted plus the evaluated statuses.
    ///
    /// This is the path ingest must use so a configured cap actually caps. Every backend that can
    /// express the check-and-insert as one critical section **must** override this — SQLite (one
    /// locked connection) and Postgres (one transaction under a per-project lock) do — so a
    /// concurrent burst cannot all read pre-burst usage and sail past the cap (check-then-act
    /// TOCTOU).
    ///
    /// The default is the documented, deliberately-named last resort
    /// [`insert_event_checked_nonatomic`]: it caps on average and leaks under contention. A backend
    /// that inherits it must leave [`Store::admission_is_atomic`] `false` so the conformance suite,
    /// the operator, and the startup log all agree the cap is advisory there.
    fn insert_event_checked(&self, ev: &LlmEvent) -> Result<Admission> {
        insert_event_checked_nonatomic(self, ev)
    }

    /// Whether [`Store::insert_event_checked`] / [`Store::insert_events_checked`] evaluate and
    /// persist in **one atomic step** on this backend, i.e. whether a configured cap is genuinely
    /// enforced under concurrent ingest rather than enforced-on-average.
    ///
    /// Defaults to `false`: a backend is advisory until it proves otherwise, so a newly-added
    /// backend can never inherit a claim it doesn't honor. The conformance suite reads this to
    /// decide whether to *require* that a concurrent burst stayed under the cap or merely to report
    /// the leak, and the API/startup surfaces it to the operator.
    fn admission_is_atomic(&self) -> bool {
        false
    }

    /// Admission-controlled **batch** ingest: evaluate + insert each event in `evs`, in order,
    /// returning one result per item (same order). Admission for item _k_ must account for the usage
    /// of every *previously-accepted* item in the same batch, so a caller cannot bypass a cap by
    /// packing many events into one request. Per-item errors (e.g. a duplicate id → `Conflict`) are
    /// returned in that item's slot rather than aborting the whole batch.
    ///
    /// The default ([`insert_events_checked_nonatomic`]) loops the per-item non-atomic path and is
    /// **not** one critical section; it inherits that path's TOCTOU leak against *concurrent*
    /// ingest, and is only cap-honest within its own batch. SQLite (one connection lock + one
    /// transaction) and Postgres (one transaction under a per-project lock) override it so the whole
    /// sequence is atomic and each accepted item is visible to the next item's usage read.
    fn insert_events_checked(&self, evs: &[LlmEvent]) -> Vec<Result<Admission>> {
        insert_events_checked_nonatomic(self, evs)
    }

    /// Most recent events, newest first, optionally filtered by project.
    fn list_events(&self, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>>;

    /// Filtered, keyset-paginated event listing (newest first). Applies the [`EventFilter`] and pages
    /// on `(ts, id)` descending, returning up to `limit` events plus a `next_cursor` when more remain.
    ///
    /// The default ignores the filter/cursor and delegates to [`Store::list_events`] (no pagination) so
    /// backends that haven't ported the keyset query compile unchanged — the SQLite backend implements
    /// the full filtered/paginated form. Correct string-keyset paging relies on the fixed-width
    /// `RFC3339(Nanos, Z)` timestamp invariant (see [`codec::fmt_ts`]).
    fn list_events_filtered(
        &self,
        project: Option<&str>,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<EventPage> {
        if let Some(what) = filter.unsupported_extension() {
            return Err(StoreError::Unsupported(what));
        }
        Ok(EventPage {
            events: self.list_events(project, limit)?,
            next_cursor: None,
            total: None,
        })
    }

    /// Cost/usage rollup grouped by project + provider + model, optionally filtered by project.
    fn cost_summary(&self, project: Option<&str>) -> Result<Vec<CostRow>>;

    /// Cost/usage rollup over an optional `[since, until)` time window (both bounds optional). The
    /// default ignores the window and delegates to [`Store::cost_summary`] (full history) so backends
    /// that haven't ported the windowed query compile unchanged; SQLite implements the window.
    fn cost_summary_windowed(
        &self,
        project: Option<&str>,
        _since: Option<DateTime<Utc>>,
        _until: Option<DateTime<Utc>>,
    ) -> Result<Vec<CostRow>> {
        self.cost_summary(project)
    }

    /// Use-case rollup: cost/usage grouped by (name, provider, model), optionally restricted to
    /// events at/after `since`. Default returns an empty rollup so backends that don't implement it
    /// (Postgres/Firestore) compile unchanged — the SQLite dev backend is the one that powers the
    /// LLM-Overview surface.
    fn usecase_costs(
        &self,
        _project: Option<&str>,
        _since: Option<DateTime<Utc>>,
    ) -> Result<Vec<UseCaseCostRow>> {
        Err(StoreError::Unsupported("the use-case cost rollup"))
    }

    /// Aggregate usage for one project since `since` (inclusive). Used by limit evaluation.
    fn usage_since(&self, project: &str, since: DateTime<Utc>) -> Result<Usage>;

    /// Aggregate usage for one project since `since`, restricted to a single dimension
    /// ([`LimitScope`]: provider / model / use-case). Used to evaluate scoped limit rules.
    ///
    /// The default **conservatively** falls back to project-wide [`Store::usage_since`] — i.e. a
    /// scoped cap on a backend that hasn't ported the scoped query counts *all* project usage against
    /// it, so it may trip early but can never silently under-enforce. Backends add a `WHERE`-clause
    /// query (SQLite does) for exact scoping; Postgres/Firestore are a documented handoff.
    fn usage_since_scoped(
        &self,
        project: &str,
        since: DateTime<Utc>,
        _scope: &LimitScope,
    ) -> Result<Usage> {
        self.usage_since(project, since)
    }

    /// Rolling usage for one project since `since`, **grouped by** every distinct value of one scope
    /// dimension (`kind` is a [`LimitScope::kind_str`]: `provider` | `model` | `name` | `api_key` |
    /// `customer`), newest-window totals per value.
    ///
    /// The pre-breach counterpart to [`Store::usage_since_scoped`]: that one answers "how much has
    /// *this* key spent", this one answers "how much has *each* key spent" — the question an operator
    /// needs answered **before** writing a per-key budget, and the one a breach makes urgent.
    ///
    /// No conservative fallback is possible here (there is no safe way to guess a grouping), so the
    /// default is an honest [`StoreError::Unsupported`] → HTTP 501 rather than an empty list that
    /// would read as "nobody spent anything".
    fn usage_by_scope(
        &self,
        _project: &str,
        _since: DateTime<Utc>,
        _kind: &str,
    ) -> Result<Vec<ScopeUsage>> {
        Err(StoreError::Unsupported("per-dimension usage breakdown"))
    }

    // --- daily time-series for predictive cost/margin forecasting ---
    // Default impls so backends that don't (yet) bucket by day compile unchanged: forecasting simply
    // reads an empty series there (no trend → no forecast) until the backend adds the queries.
    /// Daily (UTC) usage totals for one project over `[since, until)`, oldest day first — the series
    /// trend forecasting fits. Days with no traffic are absent (the caller densifies to zero).
    fn daily_usage(
        &self,
        _project: &str,
        _since: DateTime<Utc>,
        _until: DateTime<Utc>,
    ) -> Result<Vec<DailyUsage>> {
        Err(StoreError::Unsupported("the daily usage series"))
    }
    /// Daily (UTC) LLM cost per billing-dimension value (`customer` | `product`, from event
    /// metadata) over `[since, until)`, for per-customer/product margin-trend forecasting.
    fn daily_cost_by_dimension(
        &self,
        _project: Option<&str>,
        _dim: &str,
        _since: DateTime<Utc>,
        _until: DateTime<Utc>,
    ) -> Result<Vec<DailyDimCost>> {
        Err(StoreError::Unsupported("the daily cost series"))
    }

    // --- projects ---
    fn create_project(&self, p: &Project) -> Result<()>;
    /// Replace a project's mutable fields (name / enabled / redaction / collective opt-in), matched by
    /// `p.id`; `id` and `created_at` are immutable. Returns `true` when a row changed, `false` when the
    /// id is unknown (the API maps that to 404).
    ///
    /// Default is a clear unimplemented error rather than a silent no-op: `redaction` is a compliance
    /// control, and an operator who tightened it must never be told "done" by a backend that dropped
    /// the write (the same stance as `update_limit_rule`).
    fn update_project(&self, _p: &Project) -> Result<bool> {
        Err(StoreError::Unsupported("updating projects"))
    }
    fn get_project(&self, id: &str) -> Result<Option<Project>>;
    fn list_projects(&self) -> Result<Vec<Project>>;

    // --- API keys ---
    fn create_api_key(&self, k: &ApiKey) -> Result<()>;
    /// Look up a key by its (non-secret) prefix, for auth. Returns even revoked keys; caller checks.
    fn find_api_key_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>>;
    /// Best-effort update of `last_used_at`.
    fn touch_api_key(&self, id: &str, when: DateTime<Utc>) -> Result<()>;
    /// Every key minted for a project (revoked ones included — the caller decides what to show), so an
    /// operator can list, audit last-use, and pick one to revoke. Default `Ok(vec![])` so unported
    /// backends compile (matching the `get_limit_rule` precedent). NEVER expose `key_hash` upward.
    fn list_api_keys(&self, _project: &str) -> Result<Vec<ApiKey>> {
        Err(StoreError::Unsupported("listing API keys"))
    }
    /// Set a key's `revoked` flag (soft — the row is kept for audit; auth already rejects a revoked
    /// key at `guards.rs`). Returns `true` when a row changed, `false` when the id is unknown (→ 404).
    /// Default is a clear unimplemented error rather than a silent no-op, so an operator on an unported
    /// backend learns the key was NOT revoked instead of believing a leaked key is dead.
    fn set_api_key_revoked(&self, _id: &str, _revoked: bool) -> Result<bool> {
        Err(StoreError::Unsupported("revoking API keys"))
    }
    /// Stamp (or clear, with `None`) a key's `expires_at`. Returns `true` when a row changed,
    /// `false` when the id is unknown (→ 404).
    ///
    /// This is what makes key *rotation* durable: the successor is minted and the predecessor is
    /// given a deadline, so the grace window closes itself. A background revoke task would be lost
    /// on the next restart — exactly when nobody is watching — and would then leave the old key
    /// live forever. Same stance as `set_api_key_revoked`: an unported backend must say so, never
    /// report a rotation it did not persist.
    fn set_api_key_expiry(&self, _id: &str, _when: Option<DateTime<Utc>>) -> Result<bool> {
        Err(StoreError::Unsupported("API-key expiry"))
    }

    // --- limit rules ---
    fn create_limit_rule(&self, r: &LimitRule) -> Result<()>;
    fn list_limit_rules(&self, project: &str, only_enabled: bool) -> Result<Vec<LimitRule>>;
    /// Fetch one rule by id (across projects — the caller is admin-gated). Default `None` so
    /// backends that haven't ported the lifecycle read compile unchanged.
    fn get_limit_rule(&self, _id: &str) -> Result<Option<LimitRule>> {
        Err(StoreError::Unsupported("limit-rule lookup"))
    }
    /// Replace a rule's mutable fields (metric/window/threshold/action/enabled — and, once ported,
    /// `warn_at`/`scope`), matched by `r.id`; `project_id` is immutable. Returns `true` when a row
    /// was updated, `false` when the id is unknown (the API maps that to 404). The default is a clear
    /// unimplemented error rather than a silent no-op, so an operator on an unported backend learns
    /// the rule was *not* changed instead of believing a cap was tightened.
    fn update_limit_rule(&self, _r: &LimitRule) -> Result<bool> {
        Err(StoreError::Unsupported("updating limit rules"))
    }
    /// Delete a rule by id. Returns `true` when a row was removed, `false` when the id is unknown
    /// (the API maps that to 404). Default is a clear unimplemented error (see `update_limit_rule`).
    fn delete_limit_rule(&self, _id: &str) -> Result<bool> {
        Err(StoreError::Unsupported("deleting limit rules"))
    }

    // --- single event lookup + scores (Phase 3) ---
    fn get_event(&self, id: &str) -> Result<Option<LlmEvent>>;
    /// Persist a judge verdict, including its structured provenance (`Score::detail`: per-dimension
    /// breakdown, agreement, sample accounting, bias/injection flags) and its benchmark-run scoping
    /// (`Score::run_id` / `Score::case_index`). Every shipped backend persists all three — a verdict
    /// that reads back without its provenance, or a case that can't say which run produced it, is a
    /// silently degraded record rather than an obviously missing one.
    fn insert_score(&self, s: &Score) -> Result<()>;
    fn list_scores(&self, project: Option<&str>, limit: usize) -> Result<Vec<Score>>;
    /// Every case result recorded for one benchmark run, in case order (`case_index`, then
    /// `created_at`; cases without an index sort last). This is the answer to "why did run 47 fail?"
    /// — the per-case verdicts, with the provenance that produced each one.
    ///
    /// `project` is the caller's authorization scope: `Some(p)` restricts to that project (a
    /// project-scoped API key), `None` reads across projects (admin). Backends apply it in the
    /// query, so it can't be forgotten by a caller.
    ///
    /// Default is a clear [`StoreError::Unsupported`] (→ 501) rather than an empty list: "this
    /// backend never stored run scoping" and "this run had no cases" are different facts, and only
    /// one of them means the run passed.
    fn list_run_scores(
        &self,
        _run_id: &str,
        _project: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<Score>> {
        Err(StoreError::Unsupported("run-scoped case results"))
    }
    /// Of the given event ids, which already carry at least one score. Scoped to **exactly these ids**
    /// — never a blind top-N of the `scores` table — so the online scorer's "skip already-scored"
    /// stays correct however large the scores table grows. Required (no default): a wrong answer here
    /// re-judges events (burning paid judge calls) or skips new ones, so every backend implements it
    /// and the conformance suite pins it. Backed by `idx_scores_event`.
    fn scored_event_ids(&self, event_ids: &[String]) -> Result<Vec<String>>;
    /// Recent events (newest first, optionally project-scoped) that do **not** yet have a score — the
    /// online scorer's work list. The default fetches a page via [`Store::list_events`] and removes the
    /// scored ones via [`Store::scored_event_ids`], which is correct and bounded on every backend (it
    /// reads scores only for the page's ids, unlike the old client-side top-1000 anti-join that
    /// silently re-judged once a project passed 1000 scores). SQL backends may override with a single
    /// `LEFT JOIN scores ... WHERE s.id IS NULL` for one round-trip.
    fn list_unscored_events(&self, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>> {
        let events = self.list_events(project, limit)?;
        let ids: Vec<String> = events.iter().map(|e| e.id.clone()).collect();
        let scored: std::collections::HashSet<String> =
            self.scored_event_ids(&ids)?.into_iter().collect();
        Ok(events
            .into_iter()
            .filter(|e| !scored.contains(&e.id))
            .collect())
    }

    // --- traces: roll events sharing a trace_id into one end-to-end view ---
    // Default impls so backends that don't (yet) index by trace compile unchanged: the listing reads
    // empty and `get_trace` composes `list_trace_events` (so any backend that can list a trace's
    // events gets a correct rollup for free, from the pure `Trace::from_events`).
    /// Whether this backend actually serves the trace surface (listing, detail, trace scores).
    ///
    /// A capability flag rather than a probe, on the same terms as [`Store::admission_is_atomic`]:
    /// the conformance suite runs the full trace semantics against a backend that declares `true`
    /// and, against one that declares `false`, asserts every trace method *refuses* with
    /// [`StoreError::Unsupported`] — so "not implemented" can never quietly become an empty page.
    /// The API surfaces the refusal as HTTP 501 `unsupported`.
    fn serves_traces(&self) -> bool {
        false
    }
    /// Compact summaries of the most recent traces (grouped by `trace_id`), newest activity first.
    fn list_traces(&self, _project: Option<&str>, _limit: usize) -> Result<Vec<TraceSummary>> {
        Err(StoreError::Unsupported("traces"))
    }
    /// Filtered, keyset-paginated trace listing (newest `ended` first). Applies the [`TraceFilter`]
    /// and pages on `(ended, trace_id)` descending, returning up to `limit` summaries plus a
    /// `next_cursor` when more remain.
    ///
    /// The default ignores the filter/cursor and delegates to [`Store::list_traces`] (no pagination),
    /// which on a backend that doesn't serve traces is itself an [`StoreError::Unsupported`] refusal —
    /// never a silently unfiltered page. SQLite and Postgres implement the full windowed/paginated
    /// form. Correct string-keyset paging relies on the fixed-width `RFC3339(Nanos, Z)` invariant.
    fn list_traces_filtered(
        &self,
        project: Option<&str>,
        _filter: &TraceFilter,
        limit: usize,
    ) -> Result<TracePage> {
        Ok(TracePage {
            traces: self.list_traces(project, limit)?,
            next_cursor: None,
        })
    }
    /// All events of one trace **within `project`**, oldest first.
    ///
    /// A `trace_id` is caller-supplied and therefore NOT a tenant boundary: two projects can pick the
    /// same natural id (`"req-1"`, a shared upstream request id), and anyone who knows an id can post
    /// an event under it. So the project filter belongs in the query, not in a post-hoc authorization
    /// check over a cross-project merge — a colliding id in another project must be invisible here,
    /// never folded into the caller's trace. `None` means "across every project" and is reserved for
    /// operator-level principals (admin/dev); a project-scoped caller always passes `Some`.
    ///
    /// At most `max_spans` events come back (the oldest, so the trace keeps its head), with
    /// [`TraceEvents::total`] reporting the trace's real span count — the detail path is otherwise
    /// unbounded, which is how one runaway loop slows every read of that trace.
    fn list_trace_events(
        &self,
        _project: Option<&str>,
        _trace_id: &str,
        _max_spans: usize,
    ) -> Result<TraceEvents> {
        Err(StoreError::Unsupported("traces"))
    }
    /// Scores attached to any event within a trace (i.e. `scores.event_id` ∈ the trace's events),
    /// scoped by `project` on the same terms as [`Store::list_trace_events`].
    fn list_trace_scores(&self, _project: Option<&str>, _trace_id: &str) -> Result<Vec<Score>> {
        Err(StoreError::Unsupported("traces"))
    }
    /// Full rollup (totals + span tree) for one trace within `project`, or `None` if it has no events
    /// there. See [`Store::list_trace_events`] for why the project scope is part of the query and why
    /// the fan-out is capped; a clipped trace carries `spans_truncated`.
    fn get_trace(
        &self,
        project: Option<&str>,
        trace_id: &str,
        max_spans: usize,
    ) -> Result<Option<Trace>> {
        let page = self.list_trace_events(project, trace_id, max_spans)?;
        Ok(Trace::from_events_bounded(page.events, page.total))
    }

    // --- benchmarks (Phase 3.5) ---
    fn create_benchmark(&self, b: &Benchmark) -> Result<()>;
    fn get_benchmark(&self, id: &str) -> Result<Option<Benchmark>>;
    fn list_benchmarks(&self, project: &str) -> Result<Vec<Benchmark>>;
    fn create_benchmark_run(&self, r: &BenchmarkRun) -> Result<()>;
    fn list_benchmark_runs(&self, benchmark_id: &str) -> Result<Vec<BenchmarkRun>>;

    // --- model prices (Phase 3.6a) ---
    fn upsert_price(&self, p: &ModelPriceRow) -> Result<()>;
    fn list_prices(&self) -> Result<Vec<ModelPriceRow>>;

    // --- datasets (Phase 3.6b) ---
    fn create_dataset(&self, d: &Dataset) -> Result<()>;
    fn get_dataset(&self, id: &str) -> Result<Option<Dataset>>;
    fn list_datasets(&self, project: &str) -> Result<Vec<Dataset>>;
    fn set_dataset_frozen(&self, id: &str, frozen: bool) -> Result<()>;
    fn create_dataset_item(&self, item: &DatasetItem) -> Result<()>;
    fn list_dataset_items(&self, dataset_id: &str) -> Result<Vec<DatasetItem>>;

    // --- rubrics (Phase 3.6c) ---
    fn create_rubric(&self, r: &Rubric) -> Result<()>;
    fn get_rubric(&self, id: &str) -> Result<Option<Rubric>>;
    fn list_rubrics(&self, project: &str) -> Result<Vec<Rubric>>;

    // --- job queue (Phase 3.6d) ---
    fn create_job(&self, j: &Job) -> Result<()>;
    /// Atomically claim the oldest queued (or stale-running) job: sets it `running`, bumps attempts.
    /// Reclaiming a stale `running` job counts a **worker death** (`stale_reclaims` + the
    /// `JOB_ERROR_WORKER_LOST` marker), never a benchmark failure. `cancelling`/`cancelled` jobs are
    /// outside the claimable set, so a cancelled run can never be restarted by the reclaim path.
    fn claim_job(&self, stale_before: DateTime<Utc>) -> Result<Option<Job>>;
    /// Ask a queued/running job to stop. `queued` → `cancelled` outright; `running` → `cancelling`,
    /// which the worker notices at its next case boundary. `Ok(None)` = no such job.
    ///
    /// Backends that cannot do this atomically must return [`StoreError::Unsupported`] (→ 501)
    /// rather than a quiet default: a cancel that silently did nothing is worse than a 501, because
    /// the operator walks away believing the spend stopped.
    fn cancel_job(&self, _id: &str) -> Result<Option<JobCancel>> {
        Err(StoreError::Unsupported("cancelling a job"))
    }
    fn update_job_progress(&self, id: &str, progress: &str) -> Result<()>;
    /// Extend the holder's lease: move `claimed_at` forward, **conditioned on it still being
    /// `fence`** and the job still live. Returns the new `claimed_at` on success, `None` when the
    /// lease is no longer this caller's (expired and reclaimed, requeued, cancelled, finished).
    ///
    /// A renewal that nobody reads gates nothing. `None` is the executor's own gate on its
    /// legitimacy and must reach its work loop and stop it: an executor that keeps working after
    /// losing its lease interleaves its effects with its successor's.
    ///
    /// Renewal is on a **timer**, not per unit of work — a loop that renews "after each item"
    /// silently stops renewing inside the one step that takes an hour, which is exactly the step
    /// during which the lease matters. And it never waits on progress: the moment liveness is
    /// conditioned on having something to report, a live-but-stuck worker reads as a dead one, and
    /// those are the two states the whole mechanism exists to tell apart.
    fn renew_job_lease(&self, _id: &str, _fence: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        Err(StoreError::Unsupported("renewing a job lease"))
    }
    /// Write a job's verdict — **conditioned on the job still being non-terminal, and (when `fence`
    /// is given) still held by this caller**.
    ///
    /// `fence` is the `claimed_at` the caller believes it holds. Pass it from every worker; `None`
    /// is for an operator/administrative finish that is not claiming to be the holder, and even
    /// then a terminal verdict is never overwritten.
    ///
    /// Returns [`JobFinish::NotHeld`] rather than an error when the write is refused, carrying the
    /// status and lease the record actually has, so a slow worker that lost the finish-line race
    /// loses it politely and can say what beat it.
    fn finish_job(
        &self,
        id: &str,
        status: &str,
        result: &Value,
        error: Option<&str>,
        fence: Option<DateTime<Utc>>,
    ) -> Result<JobFinish>;
    fn get_job(&self, id: &str) -> Result<Option<Job>>;
    fn list_jobs(&self, status: Option<&str>, limit: usize) -> Result<Vec<Job>>;

    // --- prompt registry (versioned prompts + label-gated promotion) ---
    // Default impls so backends that don't (yet) host the registry compile unchanged: writes are a
    // clear error rather than a silent drop, and reads are empty/None.
    /// Register a new named prompt (with its initial labels/benchmark link).
    fn create_prompt(&self, _p: &Prompt) -> Result<()> {
        Err(StoreError::Unsupported("the prompt registry"))
    }
    /// Update a prompt's mutable fields (label pointers, linked benchmark, `updated_at`).
    fn update_prompt(&self, _p: &Prompt) -> Result<()> {
        Err(StoreError::Unsupported("the prompt registry"))
    }
    /// Look up a prompt by its registry name within a project (the runtime fetch path).
    fn get_prompt(&self, _project: &str, _name: &str) -> Result<Option<Prompt>> {
        Err(StoreError::Unsupported("the prompt registry"))
    }
    fn get_prompt_by_id(&self, _id: &str) -> Result<Option<Prompt>> {
        Err(StoreError::Unsupported("the prompt registry"))
    }
    fn list_prompts(&self, _project: &str) -> Result<Vec<Prompt>> {
        Err(StoreError::Unsupported("the prompt registry"))
    }
    /// Append an immutable version to a prompt.
    fn create_prompt_version(&self, _v: &PromptVersion) -> Result<()> {
        Err(StoreError::Unsupported("the prompt registry"))
    }
    fn get_prompt_version(&self, _prompt_id: &str, _version: u32) -> Result<Option<PromptVersion>> {
        Err(StoreError::Unsupported("the prompt registry"))
    }
    /// All versions of a prompt, newest version first.
    fn list_prompt_versions(&self, _prompt_id: &str) -> Result<Vec<PromptVersion>> {
        Err(StoreError::Unsupported("the prompt registry"))
    }

    // --- revenue + margin (Phase 1 profit tracking) ---
    // Default impls so backends that don't (yet) support profit tracking compile unchanged: cost is a
    // no-op (empty), and inserting revenue is a clear error rather than a silent drop.
    /// Persist one normalized revenue record.
    fn insert_revenue_event(&self, _ev: &RevenueEvent) -> Result<()> {
        Err(StoreError::Unsupported("revenue tracking"))
    }
    /// Persist a batch of revenue records **atomically** — all-or-nothing. A webhook delivery carries
    /// many events; if one fails a constraint mid-batch, none may be committed, or the provider's
    /// retry would re-fail on the same record and the events after it would be lost permanently (the
    /// handler returns an error, so 1..N-1 are already committed while N..end never land). The default
    /// loops over [`Store::insert_revenue_event`] and is **not** atomic; backends whose writes share a
    /// single critical section (e.g. SQLite) override it to wrap the batch in one transaction.
    fn insert_revenue_events(&self, evs: &[RevenueEvent]) -> Result<()> {
        for ev in evs {
            self.insert_revenue_event(ev)?;
        }
        Ok(())
    }
    /// Revenue records that may be recognized within `[since, until)`, optionally scoped to a project.
    fn list_revenue_events(
        &self,
        _project: Option<&str>,
        _since: DateTime<Utc>,
        _until: DateTime<Utc>,
    ) -> Result<Vec<RevenueEvent>> {
        Err(StoreError::Unsupported("revenue tracking"))
    }
    /// LLM cost grouped by a billing dimension (`customer` | `product`, from event metadata) over
    /// `[since, until)`.
    fn cost_by_dimension(
        &self,
        _project: Option<&str>,
        _dim: &str,
        _since: DateTime<Utc>,
        _until: DateTime<Utc>,
    ) -> Result<Vec<CostByDimension>> {
        Err(StoreError::Unsupported("cost by dimension"))
    }
    /// Prompt+completion tokens grouped by a billing dimension (`customer` | `product`, from event
    /// metadata) over `[since, until)` — the usage side of the pricing what-if simulator. Default empty
    /// so unported backends (Postgres/Firestore) compile unchanged; SQLite implements it.
    fn tokens_by_dimension(
        &self,
        _project: Option<&str>,
        _dim: &str,
        _since: DateTime<Utc>,
        _until: DateTime<Utc>,
    ) -> Result<Vec<TokensByDimension>> {
        Err(StoreError::Unsupported("token usage by dimension"))
    }
    /// One customer's LLM cost broken down **by model** (`provider/model`) over `[since, until)`,
    /// scoped by `json_extract(metadata,'$.customer_id') = customer`. Default empty so unported
    /// backends (Postgres/Firestore) compile unchanged; SQLite implements it.
    fn customer_cost_by_model(
        &self,
        _project: Option<&str>,
        _customer: &str,
        _since: DateTime<Utc>,
        _until: DateTime<Utc>,
    ) -> Result<Vec<CustomerCostRow>> {
        Err(StoreError::Unsupported("customer cost breakdown"))
    }
    /// One customer's LLM cost broken down **by use-case `name`** over `[since, until)`, scoped by the
    /// same `metadata.customer_id`. Default empty (see [`Store::customer_cost_by_model`]).
    fn customer_cost_by_name(
        &self,
        _project: Option<&str>,
        _customer: &str,
        _since: DateTime<Utc>,
        _until: DateTime<Utc>,
    ) -> Result<Vec<CustomerCostRow>> {
        Err(StoreError::Unsupported("customer cost breakdown"))
    }

    // --- cloud→device relay queue (docs/RELAY.md) ---
    // Default impls so backends that don't (yet) host the relay compile unchanged: writes are a
    // clear error rather than a silent drop, and reads/leases are empty/None.
    /// Enqueue one device task.
    fn create_relay_task(&self, _t: &RelayTask) -> Result<()> {
        Err(StoreError::Unsupported("the relay queue"))
    }
    fn get_relay_task(&self, _id: &str) -> Result<Option<RelayTask>> {
        Err(StoreError::Unsupported("the relay queue"))
    }
    /// Dedupe lookup for idempotent enqueue: the task holding `key` within `project`, if any.
    fn find_relay_task_by_key(&self, _project: &str, _key: &str) -> Result<Option<RelayTask>> {
        Err(StoreError::Unsupported("the relay queue"))
    }
    fn list_relay_tasks(
        &self,
        _project: Option<&str>,
        _status: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<RelayTask>> {
        Err(StoreError::Unsupported("the relay queue"))
    }
    /// Atomically lease up to `max` due tasks for `device`: queued tasks past `next_attempt_at`
    /// plus expired leases with attempts to spare (each lease consumes an attempt).
    fn lease_relay_tasks(
        &self,
        _device: &str,
        _lease_secs: i64,
        _max: usize,
    ) -> Result<Vec<RelayTask>> {
        Err(StoreError::Unsupported("the relay queue"))
    }
    /// Dead-letter expired leases with exhausted attempts, returning the newly-dead tasks (for
    /// alerting). The API runs this before each lease.
    fn sweep_relay_dead(&self) -> Result<Vec<RelayTask>> {
        Err(StoreError::Unsupported("the relay queue"))
    }
    /// Settle a leased task with the device's outcome; returns the updated row (`None` if the id is
    /// unknown). Settling a task that is no longer leased returns it unchanged, so a duplicate
    /// result report is harmless.
    fn settle_relay_task(&self, _id: &str, _outcome: &RelayOutcome) -> Result<Option<RelayTask>> {
        Err(StoreError::Unsupported("the relay queue"))
    }

    // --- collective model intelligence (network effect) ---
    // Default impls so backends that don't (yet) host a leaderboard compile unchanged: ingest is a
    // clear error rather than a silent drop, and the leaderboard reads as empty.
    /// Upsert one privacy-safe digest entry received from a contributor (keyed on
    /// contributor_id + provider + model + task_type).
    fn upsert_collective_entry(&self, _e: &CollectiveEntry) -> Result<()> {
        Err(StoreError::Unsupported("the collective leaderboard"))
    }
    /// Drop all of a contributor's entries (so a re-contribution replaces, never accretes, its set).
    fn delete_collective_entries(&self, _contributor_id: &str) -> Result<u64> {
        Err(StoreError::Unsupported("the collective leaderboard"))
    }
    /// All stored digest entries, for merging into the public leaderboard.
    fn list_collective_entries(&self) -> Result<Vec<CollectiveEntry>> {
        Err(StoreError::Unsupported("the collective leaderboard"))
    }
    /// Physically delete entries received before `cutoff` (retention sweep); returns how many went.
    /// A backend that leaves this unimplemented still honors the retention policy — the API filters
    /// expired entries out of the leaderboard before merging, on every backend — it just keeps the
    /// dead rows on disk. The API therefore treats `Unsupported` here as non-fatal.
    fn purge_collective_entries_before(&self, _cutoff: DateTime<Utc>) -> Result<u64> {
        Err(StoreError::Unsupported("the collective leaderboard"))
    }

    // --- storage accounting + lossless maintenance ---
    //
    // Both are `Unsupported` by default rather than returning an empty report: a managed backend
    // (Postgres, Firestore) has a disk somebody else monitors, and answering "0 bytes, no tables"
    // for it would be a confident lie in exactly the surface an operator consults about disk.

    /// Per-object disk accounting for this store. Cheap enough to serve on demand; it walks the
    /// engine's page accounting, so it is a read, not an estimate — see [`ByteMeasure`].
    fn storage_report(&self) -> Result<StorageReport> {
        Err(StoreError::Unsupported("storage accounting"))
    }

    /// Run one **lossless** maintenance chunk: checkpoint the journal, return already-freed pages.
    /// Never deletes a row (there is no pruning parameter on purpose — see `MaintenanceRequest`).
    /// The caller owns the activity gate and the chunk loop; this is one chunk.
    fn maintenance_pass(&self, _req: MaintenanceRequest) -> Result<MaintenancePass> {
        Err(StoreError::Unsupported("store maintenance"))
    }

    /// What this store has observed about its own operation latency, keyed by operation family.
    ///
    /// `Unsupported` by default for the same reason as the two above: a backend that measures
    /// nothing must say so, because an empty report reads as "everything is fast".
    fn db_metrics(&self) -> Result<DbMetricsReport> {
        Err(StoreError::Unsupported("store self-instrumentation"))
    }
}
