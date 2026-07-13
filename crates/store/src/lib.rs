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
    scope_matches, ApiKey, Benchmark, BenchmarkRun, CollectiveEntry, CostByDimension, Dataset,
    DatasetItem, Job, LimitMetric, LimitRule, LimitScope, LimitStatus, LimitWindow, LlmEvent,
    ModelPriceRow, Project, Prompt, PromptVersion, RelayOutcome, RelayTask, RevenueEvent, Rubric,
    Score, TokensByDimension, Trace, TraceSummary,
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
    pub cursor: Option<String>,
}

/// One page of events plus the cursor to fetch the next page (newest-first). `next_cursor` is `Some`
/// only when more rows exist beyond this page; pass it back as [`EventFilter::cursor`] to continue.
#[derive(Debug, Clone)]
pub struct EventPage {
    pub events: Vec<LlmEvent>,
    pub next_cursor: Option<String>,
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
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Usage {
    pub cost_usd: f64,
    pub calls: i64,
    pub tokens: i64,
}

impl Usage {
    /// The value of `metric` in this snapshot, as the comparable `f64` limits evaluate against.
    pub fn metric_value(&self, metric: LimitMetric) -> f64 {
        match metric {
            LimitMetric::CostUsd => self.cost_usd,
            LimitMetric::Calls => self.calls as f64,
            LimitMetric::Tokens => self.tokens as f64,
        }
    }

    /// Sum two usage snapshots (e.g. rolling usage plus one candidate event's contribution).
    pub fn plus(self, other: Usage) -> Usage {
        Usage {
            cost_usd: self.cost_usd + other.cost_usd,
            calls: self.calls + other.calls,
            tokens: self.tokens + other.tokens,
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
        }
    }
}

/// Outcome of an admission-controlled ingest ([`Store::insert_event_checked`]).
#[derive(Debug, Clone)]
pub struct Admission {
    /// Whether the event was persisted. `false` means a breached enforcing limit
    /// (`Throttle`/`Block`) rejected it — the API surfaces this as HTTP 429.
    pub admitted: bool,
    /// Limit statuses evaluated against rolling usage *including* the candidate event.
    pub statuses: Vec<LimitStatus>,
}

impl Admission {
    /// Admit unless a breached enforcing rule is present.
    pub(crate) fn from_statuses(statuses: Vec<LimitStatus>) -> Self {
        let admitted = !statuses.iter().any(|s| s.rejects_ingest());
        Admission { admitted, statuses }
    }
}

/// One event's contribution to rolling usage: one call, its cost, and its prompt+completion tokens
/// (matching `usage_since`, which sums `input + output`).
pub(crate) fn event_contribution(ev: &LlmEvent) -> Usage {
    Usage {
        cost_usd: ev.cost_usd.unwrap_or(0.0),
        calls: 1,
        tokens: (ev.usage.input + ev.usage.output) as i64,
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
pub(crate) fn evaluate_admission<F>(
    rules: &[LimitRule],
    ev: &LlmEvent,
    contribution: Usage,
    mut current_usage: F,
) -> Result<Admission>
where
    F: FnMut(LimitWindow, Option<&LimitScope>) -> Result<Usage>,
{
    let (provider, model, name) = (ev.provider.as_str(), ev.model.as_str(), ev.name.as_deref());
    // Usage cache now keys by (window, scope): a scoped cap and a project-wide cap over the same
    // window read different rolling totals.
    let mut prospective: HashMap<(LimitWindow, Option<LimitScope>), Usage> = HashMap::new();
    let mut statuses = Vec::new();
    for r in rules {
        if !scope_matches(r.scope.as_ref(), provider, model, name) {
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
        statuses.push(r.evaluate(usage.metric_value(r.metric)));
    }
    Ok(Admission::from_statuses(statuses))
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
    /// This is the path ingest must use so a configured cap actually caps. The default
    /// implementation composes `list_limit_rules` + `usage_since` + `insert_event` and is **not**
    /// atomic against concurrent ingest. Backends whose store call is a single critical section
    /// (e.g. SQLite, which serializes all access through one locked connection) override it to make
    /// the check-and-insert one atomic step, so a concurrent burst cannot all read pre-burst usage
    /// and sail past the cap (check-then-act TOCTOU).
    fn insert_event_checked(&self, ev: &LlmEvent) -> Result<Admission> {
        let rules = self.list_limit_rules(&ev.project_id, true)?;
        let now = Utc::now();
        let admission = evaluate_admission(&rules, ev, event_contribution(ev), |w, scope| match scope
        {
            None => self.usage_since(&ev.project_id, w.since(now)),
            Some(s) => self.usage_since_scoped(&ev.project_id, w.since(now), s),
        })?;
        if admission.admitted {
            self.insert_event(ev)?;
        }
        Ok(admission)
    }

    /// Admission-controlled **batch** ingest: evaluate + insert each event in `evs`, in order,
    /// returning one result per item (same order). Admission for item _k_ must account for the usage
    /// of every *previously-accepted* item in the same batch, so a caller cannot bypass a cap by
    /// packing many events into one request. Per-item errors (e.g. a duplicate id → `Conflict`) are
    /// returned in that item's slot rather than aborting the whole batch.
    ///
    /// The default loops [`Store::insert_event_checked`] and is **not** one critical section (each
    /// item is its own transaction); it is nonetheless cap-honest for backends that commit each
    /// insert before the next admission read (Postgres/Firestore stance — a single-transaction port
    /// is a handoff). SQLite overrides it to hold its one connection lock for the whole batch so the
    /// entire check-and-insert sequence is atomic (previously-accepted items are already visible to
    /// the next item's usage read).
    fn insert_events_checked(&self, evs: &[LlmEvent]) -> Vec<Result<Admission>> {
        evs.iter().map(|e| self.insert_event_checked(e)).collect()
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
        _filter: &EventFilter,
        limit: usize,
    ) -> Result<EventPage> {
        Ok(EventPage {
            events: self.list_events(project, limit)?,
            next_cursor: None,
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
        Ok(Vec::new())
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
        Ok(Vec::new())
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
        Ok(Vec::new())
    }

    // --- projects ---
    fn create_project(&self, p: &Project) -> Result<()>;
    fn get_project(&self, id: &str) -> Result<Option<Project>>;
    fn list_projects(&self) -> Result<Vec<Project>>;

    // --- API keys ---
    fn create_api_key(&self, k: &ApiKey) -> Result<()>;
    /// Look up a key by its (non-secret) prefix, for auth. Returns even revoked keys; caller checks.
    fn find_api_key_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>>;
    /// Best-effort update of `last_used_at`.
    fn touch_api_key(&self, id: &str, when: DateTime<Utc>) -> Result<()>;

    // --- limit rules ---
    fn create_limit_rule(&self, r: &LimitRule) -> Result<()>;
    fn list_limit_rules(&self, project: &str, only_enabled: bool) -> Result<Vec<LimitRule>>;
    /// Fetch one rule by id (across projects — the caller is admin-gated). Default `None` so
    /// backends that haven't ported the lifecycle read compile unchanged.
    fn get_limit_rule(&self, _id: &str) -> Result<Option<LimitRule>> {
        Ok(None)
    }
    /// Replace a rule's mutable fields (metric/window/threshold/action/enabled — and, once ported,
    /// `warn_at`/`scope`), matched by `r.id`; `project_id` is immutable. Returns `true` when a row
    /// was updated, `false` when the id is unknown (the API maps that to 404). The default is a clear
    /// unimplemented error rather than a silent no-op, so an operator on an unported backend learns
    /// the rule was *not* changed instead of believing a cap was tightened.
    fn update_limit_rule(&self, _r: &LimitRule) -> Result<bool> {
        Err(StoreError::Other(
            "updating limit rules is not supported by this store backend".to_string(),
        ))
    }
    /// Delete a rule by id. Returns `true` when a row was removed, `false` when the id is unknown
    /// (the API maps that to 404). Default is a clear unimplemented error (see `update_limit_rule`).
    fn delete_limit_rule(&self, _id: &str) -> Result<bool> {
        Err(StoreError::Other(
            "deleting limit rules is not supported by this store backend".to_string(),
        ))
    }

    // --- single event lookup + scores (Phase 3) ---
    fn get_event(&self, id: &str) -> Result<Option<LlmEvent>>;
    fn insert_score(&self, s: &Score) -> Result<()>;
    fn list_scores(&self, project: Option<&str>, limit: usize) -> Result<Vec<Score>>;

    // --- traces: roll events sharing a trace_id into one end-to-end view ---
    // Default impls so backends that don't (yet) index by trace compile unchanged: the listing reads
    // empty and `get_trace` composes `list_trace_events` (so any backend that can list a trace's
    // events gets a correct rollup for free, from the pure `Trace::from_events`).
    /// Compact summaries of the most recent traces (grouped by `trace_id`), newest activity first.
    fn list_traces(&self, _project: Option<&str>, _limit: usize) -> Result<Vec<TraceSummary>> {
        Ok(Vec::new())
    }
    /// Filtered, keyset-paginated trace listing (newest `ended` first). Applies the [`TraceFilter`]
    /// and pages on `(ended, trace_id)` descending, returning up to `limit` summaries plus a
    /// `next_cursor` when more remain.
    ///
    /// The default ignores the filter/cursor and delegates to [`Store::list_traces`] (no pagination)
    /// so backends that only return empty traces (Postgres/Firestore) compile unchanged — the SQLite
    /// backend, which owns the trace surface, implements the full windowed/paginated form. Correct
    /// string-keyset paging relies on the fixed-width `RFC3339(Nanos, Z)` timestamp invariant.
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
    /// All events of one trace, regardless of project (the caller authorizes against the result).
    fn list_trace_events(&self, _trace_id: &str) -> Result<Vec<LlmEvent>> {
        Ok(Vec::new())
    }
    /// Scores attached to any event within a trace (i.e. `scores.event_id` ∈ the trace's events).
    fn list_trace_scores(&self, _trace_id: &str) -> Result<Vec<Score>> {
        Ok(Vec::new())
    }
    /// Full rollup (totals + span tree) for one trace, or `None` if it has no events.
    fn get_trace(&self, trace_id: &str) -> Result<Option<Trace>> {
        Ok(Trace::from_events(self.list_trace_events(trace_id)?))
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
    fn claim_job(&self, stale_before: DateTime<Utc>) -> Result<Option<Job>>;
    fn update_job_progress(&self, id: &str, progress: &str) -> Result<()>;
    fn finish_job(&self, id: &str, status: &str, result: &Value, error: Option<&str>) -> Result<()>;
    fn get_job(&self, id: &str) -> Result<Option<Job>>;
    fn list_jobs(&self, status: Option<&str>, limit: usize) -> Result<Vec<Job>>;

    // --- prompt registry (versioned prompts + label-gated promotion) ---
    // Default impls so backends that don't (yet) host the registry compile unchanged: writes are a
    // clear error rather than a silent drop, and reads are empty/None.
    /// Register a new named prompt (with its initial labels/benchmark link).
    fn create_prompt(&self, _p: &Prompt) -> Result<()> {
        Err(StoreError::Other(
            "prompt registry is not supported by this store backend".to_string(),
        ))
    }
    /// Update a prompt's mutable fields (label pointers, linked benchmark, `updated_at`).
    fn update_prompt(&self, _p: &Prompt) -> Result<()> {
        Err(StoreError::Other(
            "prompt registry is not supported by this store backend".to_string(),
        ))
    }
    /// Look up a prompt by its registry name within a project (the runtime fetch path).
    fn get_prompt(&self, _project: &str, _name: &str) -> Result<Option<Prompt>> {
        Ok(None)
    }
    fn get_prompt_by_id(&self, _id: &str) -> Result<Option<Prompt>> {
        Ok(None)
    }
    fn list_prompts(&self, _project: &str) -> Result<Vec<Prompt>> {
        Ok(Vec::new())
    }
    /// Append an immutable version to a prompt.
    fn create_prompt_version(&self, _v: &PromptVersion) -> Result<()> {
        Err(StoreError::Other(
            "prompt registry is not supported by this store backend".to_string(),
        ))
    }
    fn get_prompt_version(&self, _prompt_id: &str, _version: u32) -> Result<Option<PromptVersion>> {
        Ok(None)
    }
    /// All versions of a prompt, newest version first.
    fn list_prompt_versions(&self, _prompt_id: &str) -> Result<Vec<PromptVersion>> {
        Ok(Vec::new())
    }

    // --- revenue + margin (Phase 1 profit tracking) ---
    // Default impls so backends that don't (yet) support profit tracking compile unchanged: cost is a
    // no-op (empty), and inserting revenue is a clear error rather than a silent drop.
    /// Persist one normalized revenue record.
    fn insert_revenue_event(&self, _ev: &RevenueEvent) -> Result<()> {
        Err(StoreError::Other(
            "revenue tracking is not supported by this store backend".to_string(),
        ))
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
        Ok(Vec::new())
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
        Ok(Vec::new())
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
        Ok(Vec::new())
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
        Ok(Vec::new())
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
        Ok(Vec::new())
    }

    // --- cloud→device relay queue (docs/RELAY.md) ---
    // Default impls so backends that don't (yet) host the relay compile unchanged: writes are a
    // clear error rather than a silent drop, and reads/leases are empty/None.
    /// Enqueue one device task.
    fn create_relay_task(&self, _t: &RelayTask) -> Result<()> {
        Err(StoreError::Other(
            "relay queue is not supported by this store backend".to_string(),
        ))
    }
    fn get_relay_task(&self, _id: &str) -> Result<Option<RelayTask>> {
        Ok(None)
    }
    /// Dedupe lookup for idempotent enqueue: the task holding `key` within `project`, if any.
    fn find_relay_task_by_key(&self, _project: &str, _key: &str) -> Result<Option<RelayTask>> {
        Ok(None)
    }
    fn list_relay_tasks(
        &self,
        _project: Option<&str>,
        _status: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<RelayTask>> {
        Ok(Vec::new())
    }
    /// Atomically lease up to `max` due tasks for `device`: queued tasks past `next_attempt_at`
    /// plus expired leases with attempts to spare (each lease consumes an attempt).
    fn lease_relay_tasks(
        &self,
        _device: &str,
        _lease_secs: i64,
        _max: usize,
    ) -> Result<Vec<RelayTask>> {
        Ok(Vec::new())
    }
    /// Dead-letter expired leases with exhausted attempts, returning the newly-dead tasks (for
    /// alerting). The API runs this before each lease.
    fn sweep_relay_dead(&self) -> Result<Vec<RelayTask>> {
        Ok(Vec::new())
    }
    /// Settle a leased task with the device's outcome; returns the updated row (`None` if the id is
    /// unknown). Settling a task that is no longer leased returns it unchanged, so a duplicate
    /// result report is harmless.
    fn settle_relay_task(&self, _id: &str, _outcome: &RelayOutcome) -> Result<Option<RelayTask>> {
        Err(StoreError::Other(
            "relay queue is not supported by this store backend".to_string(),
        ))
    }

    // --- collective model intelligence (network effect) ---
    // Default impls so backends that don't (yet) host a leaderboard compile unchanged: ingest is a
    // clear error rather than a silent drop, and the leaderboard reads as empty.
    /// Upsert one privacy-safe digest entry received from a contributor (keyed on
    /// contributor_id + provider + model + task_type).
    fn upsert_collective_entry(&self, _e: &CollectiveEntry) -> Result<()> {
        Err(StoreError::Other(
            "collective leaderboard is not supported by this store backend".to_string(),
        ))
    }
    /// Drop all of a contributor's entries (so a re-contribution replaces, never accretes, its set).
    fn delete_collective_entries(&self, _contributor_id: &str) -> Result<u64> {
        Ok(0)
    }
    /// All stored digest entries, for merging into the public leaderboard.
    fn list_collective_entries(&self) -> Result<Vec<CollectiveEntry>> {
        Ok(Vec::new())
    }
}
