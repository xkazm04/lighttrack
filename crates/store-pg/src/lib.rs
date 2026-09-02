//! Postgres backend for LightTrack — implements the [`lighttrack_store::Store`] trait via `sqlx`,
//! so the same app runs on any managed Postgres (RDS / Cloud SQL / Azure DB / Neon / Supabase).
//!
//! The `Store` trait is synchronous (the SQLite backend is blocking); `sqlx` is async, so `PgStore`
//! owns a small Tokio runtime and `block_on`s each query. Callers already invoke store methods from
//! `spawn_blocking`, so this never blocks the API's async workers.
//!
//! Implements the full `Store` trait, verified against Postgres. This file is wiring: `connect` +
//! the `impl Store` that delegates each method to an `async fn` in a per-domain module (`events`,
//! `scores`, `projects`, `prices`, `benchmarks`, `datasets`, `rubrics`, `jobs`, `revenue`,
//! `relay`, `traces`), mirroring the SQLite backend's layout. `claim_job` and the relay `lease` use
//! `FOR UPDATE SKIP LOCKED … RETURNING` for concurrency-safe atomic dequeues.

mod admission;
mod benchmarks;
mod collective;
mod datasets;
mod events;
mod jobs;
mod margin_policies;
mod prices;
mod projects;
mod prompts;
mod redaction;
mod relay;
mod relay_lease;
mod revenue;
mod rollup;
mod rubrics;
mod schedules;
mod scores;
mod traces;
mod util;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::runtime::Runtime;

use lighttrack_core::{
    ApiKey, Benchmark, BenchmarkRun, CollectiveEntry, CostByDimension, Dataset, DatasetItem, Job,
    JobCancel, JobFinish, LeaseHeld, LimitRule, LimitScope, LlmEvent, ModelPriceRow, Project,
    Prompt, PromptVersion, RelayCancel, RelayOutcome, RelaySettle, RelayTask, RevenueEvent,
    RollupQuery, RollupRow, Rubric, Schedule, Score, TraceSummary,
};
use lighttrack_store::{
    capabilities::{Capabilities, Surface},
    Admission, CollectiveFilter, CostRow, EventFilter, EventPage, RedactionPostureRow, ReplaceAck,
    RepriceReport, Result, ScopeUsage, ScoreFilter, Store, StoreError, TraceEvents, TraceFilter,
    TracePage, Usage, UseCaseCostRow,
};

use util::pgerr;

const SCHEMA: &str = include_str!("../../../schema/postgres/001_init.sql");

/// Postgres-backed [`Store`].
pub struct PgStore {
    pool: PgPool,
    rt: Runtime,
}

impl PgStore {
    /// Connect (sslmode=prefer by default: TLS for cloud, plaintext fallback for local Docker) and
    /// ensure the schema exists.
    pub fn connect(database_url: &str) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| StoreError::Other(format!("tokio runtime: {e}")))?;
        let pool = rt
            .block_on(async {
                PgPoolOptions::new()
                    .max_connections(5)
                    .connect(database_url)
                    .await
            })
            .map_err(pgerr)?;
        let store = Self { pool, rt };
        store.init_schema()?;
        Ok(store)
    }
}

impl PgStore {
    /// What this backend implements today, read off the `impl Store` block below.
    ///
    /// `Rollup` is the primitive the forecast and margin surfaces read through: implementing it
    /// once is what makes `Forecast` and `MarginBreakdowns` answer here (their methods default over
    /// it), instead of the 501s `/v1/forecast` and three `/v1/margin/*` routes used to return.
    ///
    /// The absent surfaces are honest gaps, not oversights: `Maintenance`/`Metrics` are SQLite-file
    /// concerns a managed Postgres owns itself. Each refuses with `Unsupported` (HTTP 501) and the
    /// conformance suite asserts that refusal.
    pub const SURFACES: &'static [Surface] = &[
        Surface::EventsCore,
        Surface::EventFilters,
        Surface::Rollup,
        Surface::Forecast,
        Surface::MarginBreakdowns,
        Surface::RedactionPosture,
        Surface::RevenueReprice,
        Surface::ScoreFilters,
        Surface::Traces,
        Surface::ProjectAdmin,
        Surface::KeyAdmin,
        Surface::LimitLifecycle,
        Surface::MarginPolicies,
        Surface::JobLeases,
        Surface::Relay,
        // The hub runs here: a managed Postgres is where a public leaderboard is actually deployed,
        // so a 501 on `/v1/collective/*` was the gap that mattered most.
        Surface::Collective,
        Surface::Schedules,
        // The prompt registry, and with it the promotion gate. It used to 501 here, which meant the
        // one place a prompt edit becomes a measurable quality step did not exist on the managed
        // deployments the product actually runs on.
        Surface::Prompts,
    ];

    /// This backend's manifest as a pure function of the type — `lighttrack-store`'s parity-doc
    /// test renders the matrix from it without a live database.
    pub fn manifest() -> Capabilities {
        // Check-and-insert is one transaction, serialized per project by a transaction-scoped
        // advisory lock — across every API process sharing the database. See `admission`.
        Capabilities::new("postgres", Self::SURFACES, true)
    }
}

impl Store for PgStore {
    fn capabilities(&self) -> Capabilities {
        Self::manifest()
    }

    fn init_schema(&self) -> Result<()> {
        self.rt
            .block_on(async { sqlx::raw_sql(SCHEMA).execute(&self.pool).await })
            .map_err(pgerr)?;
        Ok(())
    }

    // --- events ---
    fn insert_event(&self, ev: &LlmEvent) -> Result<()> {
        self.rt.block_on(events::insert(&self.pool, ev))
    }
    fn insert_event_checked(&self, ev: &LlmEvent) -> Result<Admission> {
        self.rt
            .block_on(admission::insert_event_checked(&self.pool, ev))
    }
    fn insert_events_checked(&self, evs: &[LlmEvent]) -> Vec<Result<Admission>> {
        self.rt
            .block_on(admission::insert_events_checked(&self.pool, evs))
    }
    fn list_events(&self, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>> {
        self.rt.block_on(events::list(&self.pool, project, limit))
    }
    fn list_events_filtered(
        &self,
        project: Option<&str>,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<EventPage> {
        self.rt
            .block_on(events::list_filtered(&self.pool, project, filter, limit))
    }
    fn cost_summary(&self, project: Option<&str>) -> Result<Vec<CostRow>> {
        self.rt.block_on(events::cost_summary(&self.pool, project))
    }
    /// The grouped-rollup primitive. Everything in the forecast and margin-breakdown surfaces
    /// reaches Postgres through the trait defaults over this one method.
    fn rollup(&self, q: &RollupQuery<'_>) -> Result<Vec<RollupRow>> {
        self.rt.block_on(rollup::rollup(&self.pool, q))
    }
    fn cost_summary_windowed(
        &self,
        project: Option<&str>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<Vec<CostRow>> {
        self.rt.block_on(events::cost_summary_windowed(
            &self.pool, project, since, until,
        ))
    }
    fn usecase_costs(
        &self,
        project: Option<&str>,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<UseCaseCostRow>> {
        self.rt
            .block_on(events::usecase_costs(&self.pool, project, since))
    }
    fn usage_since(&self, project: &str, since: DateTime<Utc>) -> Result<Usage> {
        self.rt
            .block_on(events::usage_since(&self.pool, project, since))
    }
    fn usage_since_scoped(
        &self,
        project: &str,
        since: DateTime<Utc>,
        scope: &LimitScope,
    ) -> Result<Usage> {
        self.rt.block_on(events::usage_since_scoped(
            &self.pool, project, since, scope,
        ))
    }
    fn usage_by_scope(
        &self,
        project: &str,
        since: DateTime<Utc>,
        kind: &str,
    ) -> Result<Vec<ScopeUsage>> {
        self.rt
            .block_on(events::usage_by_scope(&self.pool, project, since, kind))
    }
    fn redaction_posture(
        &self,
        project: Option<&str>,
        since: DateTime<Utc>,
    ) -> Result<Vec<RedactionPostureRow>> {
        self.rt
            .block_on(redaction::posture(&self.pool, project, since))
    }
    fn get_event(&self, id: &str) -> Result<Option<LlmEvent>> {
        self.rt.block_on(events::get(&self.pool, id))
    }

    // --- traces ---
    // Implemented, not inherited: the trait defaults refuse with `Unsupported` (HTTP 501), which on
    // the backend most deployments actually run meant the whole trace surface was missing. Semantics
    // are ported from the SQLite reference — see `traces`.
    fn list_traces(&self, project: Option<&str>, limit: usize) -> Result<Vec<TraceSummary>> {
        self.rt
            .block_on(traces::list_summaries(&self.pool, project, limit))
    }
    fn list_traces_filtered(
        &self,
        project: Option<&str>,
        filter: &TraceFilter,
        limit: usize,
    ) -> Result<TracePage> {
        self.rt.block_on(traces::list_summaries_filtered(
            &self.pool, project, filter, limit,
        ))
    }
    fn list_trace_events(
        &self,
        project: Option<&str>,
        trace_id: &str,
        max_spans: usize,
    ) -> Result<TraceEvents> {
        self.rt.block_on(traces::list_by_trace(
            &self.pool, project, trace_id, max_spans,
        ))
    }
    fn list_trace_scores(&self, project: Option<&str>, trace_id: &str) -> Result<Vec<Score>> {
        self.rt
            .block_on(traces::list_scores_by_trace(&self.pool, project, trace_id))
    }

    // --- projects / api keys / limits ---
    fn create_project(&self, p: &Project) -> Result<()> {
        self.rt.block_on(projects::create(&self.pool, p))
    }
    fn update_project(&self, p: &Project) -> Result<bool> {
        self.rt.block_on(projects::update(&self.pool, p))
    }
    fn get_project(&self, id: &str) -> Result<Option<Project>> {
        self.rt.block_on(projects::get(&self.pool, id))
    }
    fn list_projects(&self) -> Result<Vec<Project>> {
        self.rt.block_on(projects::list(&self.pool))
    }
    fn create_api_key(&self, k: &ApiKey) -> Result<()> {
        self.rt.block_on(projects::create_key(&self.pool, k))
    }
    fn find_api_key_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>> {
        self.rt
            .block_on(projects::find_key_by_prefix(&self.pool, prefix))
    }
    fn touch_api_key(&self, id: &str, when: DateTime<Utc>) -> Result<()> {
        self.rt.block_on(projects::touch_key(&self.pool, id, when))
    }
    fn list_api_keys(&self, project: &str) -> Result<Vec<ApiKey>> {
        self.rt.block_on(projects::list_keys(&self.pool, project))
    }
    fn set_api_key_revoked(&self, id: &str, revoked: bool) -> Result<bool> {
        self.rt
            .block_on(projects::set_key_revoked(&self.pool, id, revoked))
    }
    fn set_api_key_expiry(&self, id: &str, when: Option<DateTime<Utc>>) -> Result<bool> {
        self.rt
            .block_on(projects::set_key_expiry(&self.pool, id, when))
    }
    fn create_limit_rule(&self, r: &LimitRule) -> Result<()> {
        self.rt.block_on(projects::create_limit(&self.pool, r))
    }
    fn list_limit_rules(&self, project: &str, only_enabled: bool) -> Result<Vec<LimitRule>> {
        self.rt
            .block_on(projects::list_limits(&self.pool, project, only_enabled))
    }
    fn get_limit_rule(&self, id: &str) -> Result<Option<LimitRule>> {
        self.rt.block_on(projects::get_limit(&self.pool, id))
    }
    fn update_limit_rule(&self, r: &LimitRule) -> Result<bool> {
        self.rt.block_on(projects::update_limit(&self.pool, r))
    }
    fn delete_limit_rule(&self, id: &str) -> Result<bool> {
        self.rt.block_on(projects::delete_limit(&self.pool, id))
    }

    // --- margin policies ---
    fn create_margin_policy(&self, p: &lighttrack_core::MarginPolicy) -> Result<()> {
        self.rt.block_on(margin_policies::create(&self.pool, p))
    }
    fn list_margin_policies(
        &self,
        project: &str,
        only_enabled: bool,
    ) -> Result<Vec<lighttrack_core::MarginPolicy>> {
        self.rt
            .block_on(margin_policies::list(&self.pool, project, only_enabled))
    }
    fn get_margin_policy(&self, id: &str) -> Result<Option<lighttrack_core::MarginPolicy>> {
        self.rt.block_on(margin_policies::get(&self.pool, id))
    }
    fn delete_margin_policy(&self, id: &str) -> Result<bool> {
        self.rt.block_on(margin_policies::delete(&self.pool, id))
    }

    // --- scores ---
    fn insert_score(&self, s: &Score) -> Result<()> {
        self.rt.block_on(scores::insert(&self.pool, s))
    }
    fn list_scores(&self, project: Option<&str>, limit: usize) -> Result<Vec<Score>> {
        self.rt.block_on(scores::list(&self.pool, project, limit))
    }
    fn list_scores_filtered(
        &self,
        project: Option<&str>,
        filter: &ScoreFilter,
        limit: usize,
    ) -> Result<Vec<Score>> {
        self.rt
            .block_on(scores::list_filtered(&self.pool, project, filter, limit))
    }
    fn list_run_scores(
        &self,
        run_id: &str,
        project: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Score>> {
        self.rt
            .block_on(scores::list_by_run(&self.pool, run_id, project, limit))
    }
    fn scored_event_ids(&self, event_ids: &[String]) -> Result<Vec<String>> {
        self.rt
            .block_on(scores::scored_event_ids(&self.pool, event_ids))
    }

    // --- prices ---
    fn upsert_price(&self, p: &ModelPriceRow) -> Result<()> {
        self.rt.block_on(prices::upsert(&self.pool, p))
    }
    fn list_prices(&self) -> Result<Vec<ModelPriceRow>> {
        self.rt.block_on(prices::list(&self.pool))
    }

    // --- benchmarks ---
    fn create_benchmark(&self, b: &Benchmark) -> Result<()> {
        self.rt.block_on(benchmarks::create(&self.pool, b))
    }
    fn get_benchmark(&self, id: &str) -> Result<Option<Benchmark>> {
        self.rt.block_on(benchmarks::get(&self.pool, id))
    }
    fn list_benchmarks(&self, project: &str) -> Result<Vec<Benchmark>> {
        self.rt.block_on(benchmarks::list(&self.pool, project))
    }
    fn create_benchmark_run(&self, r: &BenchmarkRun) -> Result<()> {
        self.rt.block_on(benchmarks::create_run(&self.pool, r))
    }
    fn list_benchmark_runs(&self, benchmark_id: &str) -> Result<Vec<BenchmarkRun>> {
        self.rt
            .block_on(benchmarks::list_runs(&self.pool, benchmark_id))
    }

    // --- datasets ---
    fn create_dataset(&self, d: &Dataset) -> Result<()> {
        self.rt.block_on(datasets::create(&self.pool, d))
    }
    fn get_dataset(&self, id: &str) -> Result<Option<Dataset>> {
        self.rt.block_on(datasets::get(&self.pool, id))
    }
    fn list_datasets(&self, project: &str) -> Result<Vec<Dataset>> {
        self.rt.block_on(datasets::list(&self.pool, project))
    }
    fn set_dataset_frozen(&self, id: &str, frozen: bool) -> Result<()> {
        self.rt
            .block_on(datasets::set_frozen(&self.pool, id, frozen))
    }
    fn create_dataset_item(&self, item: &DatasetItem) -> Result<()> {
        self.rt.block_on(datasets::create_item(&self.pool, item))
    }
    fn list_dataset_items(&self, dataset_id: &str) -> Result<Vec<DatasetItem>> {
        self.rt
            .block_on(datasets::list_items(&self.pool, dataset_id))
    }

    // --- rubrics ---
    fn create_rubric(&self, r: &Rubric) -> Result<()> {
        self.rt.block_on(rubrics::create(&self.pool, r))
    }
    fn get_rubric(&self, id: &str) -> Result<Option<Rubric>> {
        self.rt.block_on(rubrics::get(&self.pool, id))
    }
    fn list_rubrics(&self, project: &str) -> Result<Vec<Rubric>> {
        self.rt.block_on(rubrics::list(&self.pool, project))
    }

    // --- jobs ---
    fn create_job(&self, j: &Job) -> Result<()> {
        self.rt.block_on(jobs::create(&self.pool, j))
    }
    fn claim_job(&self, stale_before: DateTime<Utc>, kinds: &[&str]) -> Result<Option<Job>> {
        self.rt
            .block_on(jobs::claim(&self.pool, stale_before, kinds))
    }
    fn cancel_job(&self, id: &str) -> Result<Option<JobCancel>> {
        self.rt.block_on(jobs::cancel(&self.pool, id))
    }
    fn update_job_progress(&self, id: &str, progress: &str) -> Result<()> {
        self.rt
            .block_on(jobs::update_progress(&self.pool, id, progress))
    }
    fn renew_job_lease(&self, id: &str, fence: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        self.rt.block_on(jobs::renew_lease(&self.pool, id, fence))
    }
    fn finish_job(
        &self,
        id: &str,
        status: &str,
        result: &Value,
        error: Option<&str>,
        fence: Option<DateTime<Utc>>,
    ) -> Result<JobFinish> {
        self.rt
            .block_on(jobs::finish(&self.pool, id, status, result, error, fence))
    }
    fn get_job(&self, id: &str) -> Result<Option<Job>> {
        self.rt.block_on(jobs::get(&self.pool, id))
    }
    fn list_jobs(&self, status: Option<&str>, limit: usize) -> Result<Vec<Job>> {
        self.rt.block_on(jobs::list(&self.pool, status, limit))
    }

    // --- stored schedules ---
    fn create_schedule(&self, s: &Schedule) -> Result<()> {
        self.rt.block_on(schedules::create(&self.pool, s))
    }
    fn get_schedule(&self, id: &str) -> Result<Option<Schedule>> {
        self.rt.block_on(schedules::get(&self.pool, id))
    }
    fn list_schedules(&self, project: &str) -> Result<Vec<Schedule>> {
        self.rt.block_on(schedules::list(&self.pool, project))
    }
    fn update_schedule(&self, s: &Schedule) -> Result<bool> {
        self.rt.block_on(schedules::update(&self.pool, s))
    }
    fn delete_schedule(&self, id: &str) -> Result<bool> {
        self.rt.block_on(schedules::delete(&self.pool, id))
    }
    fn due_schedules(&self, now: DateTime<Utc>) -> Result<Vec<Schedule>> {
        self.rt.block_on(schedules::due(&self.pool, now))
    }

    // --- cloud→device relay queue ---
    fn create_relay_task(&self, t: &RelayTask) -> Result<()> {
        self.rt.block_on(relay::create(&self.pool, t))
    }
    fn get_relay_task(&self, id: &str) -> Result<Option<RelayTask>> {
        self.rt.block_on(relay::get(&self.pool, id))
    }
    fn find_relay_task_by_key(&self, project: &str, key: &str) -> Result<Option<RelayTask>> {
        self.rt
            .block_on(relay::find_by_key(&self.pool, project, key))
    }
    fn list_relay_tasks(
        &self,
        project: Option<&str>,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RelayTask>> {
        self.rt
            .block_on(relay::list(&self.pool, project, status, limit))
    }
    fn lease_relay_tasks(
        &self,
        device: &str,
        lease_secs: i64,
        max: usize,
    ) -> Result<Vec<RelayTask>> {
        self.rt
            .block_on(relay::lease(&self.pool, device, lease_secs, max))
    }
    fn sweep_relay_dead(&self) -> Result<Vec<RelayTask>> {
        self.rt.block_on(relay::sweep_dead(&self.pool))
    }
    fn settle_relay_task(
        &self,
        id: &str,
        fence: Option<DateTime<Utc>>,
        outcome: &RelayOutcome,
    ) -> Result<RelaySettle> {
        self.rt
            .block_on(relay_lease::settle(&self.pool, id, fence, outcome))
    }
    fn renew_relay_lease(
        &self,
        id: &str,
        fence: DateTime<Utc>,
        lease_secs: i64,
    ) -> Result<LeaseHeld> {
        self.rt
            .block_on(relay_lease::renew(&self.pool, id, fence, lease_secs))
    }
    fn update_relay_progress(
        &self,
        id: &str,
        fence: DateTime<Utc>,
        progress: &str,
    ) -> Result<LeaseHeld> {
        self.rt.block_on(relay_lease::update_progress(
            &self.pool, id, fence, progress,
        ))
    }
    fn cancel_relay_task(&self, id: &str) -> Result<Option<RelayCancel>> {
        self.rt.block_on(relay_lease::cancel(&self.pool, id))
    }

    // --- revenue + margin (profit tracking) ---
    fn insert_revenue_event(&self, ev: &RevenueEvent) -> Result<()> {
        self.rt.block_on(revenue::insert(&self.pool, ev))
    }
    fn list_revenue_events(
        &self,
        project: Option<&str>,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<RevenueEvent>> {
        self.rt
            .block_on(revenue::list(&self.pool, project, since, until))
    }
    fn reprice_revenue(
        &self,
        project: Option<&str>,
        currency: &str,
        rate: f64,
        version: &str,
        dry_run: bool,
    ) -> Result<RepriceReport> {
        self.rt.block_on(revenue::reprice(
            &self.pool, project, currency, rate, version, dry_run,
        ))
    }
    fn cost_by_dimension(
        &self,
        project: Option<&str>,
        dim: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<CostByDimension>> {
        self.rt.block_on(revenue::cost_by_dimension(
            &self.pool, project, dim, since, until,
        ))
    }

    // --- collective model intelligence (the shared leaderboard) ---
    fn upsert_collective_entry(&self, e: &CollectiveEntry) -> Result<()> {
        self.rt.block_on(collective::upsert(&self.pool, e))
    }
    fn delete_collective_entries(&self, contributor_id: &str) -> Result<u64> {
        self.rt
            .block_on(collective::delete(&self.pool, contributor_id))
    }
    fn list_collective_entries(&self) -> Result<Vec<CollectiveEntry>> {
        self.rt.block_on(collective::list(&self.pool))
    }
    fn purge_collective_entries_before(&self, cutoff: DateTime<Utc>) -> Result<u64> {
        self.rt
            .block_on(collective::purge_before(&self.pool, cutoff))
    }
    fn replace_collective_contribution(
        &self,
        contributor_id: &str,
        entries: &[CollectiveEntry],
        purge_before: Option<DateTime<Utc>>,
    ) -> Result<ReplaceAck> {
        self.rt.block_on(collective::replace(
            &self.pool,
            contributor_id,
            entries,
            purge_before,
        ))
    }
    fn latest_collective_receipt(&self, contributor_id: &str) -> Result<Option<DateTime<Utc>>> {
        self.rt
            .block_on(collective::latest_receipt(&self.pool, contributor_id))
    }
    fn list_collective_entries_filtered(
        &self,
        f: &CollectiveFilter,
    ) -> Result<Vec<CollectiveEntry>> {
        self.rt.block_on(collective::list_filtered(&self.pool, f))
    }

    // --- prompt registry (M10) ---
    fn create_prompt(&self, p: &Prompt) -> Result<()> {
        self.rt.block_on(prompts::create(&self.pool, p))
    }
    fn update_prompt(&self, p: &Prompt) -> Result<()> {
        self.rt.block_on(prompts::update(&self.pool, p))
    }
    fn get_prompt(&self, project: &str, name: &str) -> Result<Option<Prompt>> {
        self.rt.block_on(prompts::get(&self.pool, project, name))
    }
    fn get_prompt_by_id(&self, id: &str) -> Result<Option<Prompt>> {
        self.rt.block_on(prompts::get_by_id(&self.pool, id))
    }
    fn list_prompts(&self, project: &str) -> Result<Vec<Prompt>> {
        self.rt.block_on(prompts::list(&self.pool, project))
    }
    fn create_prompt_version(&self, v: &PromptVersion) -> Result<()> {
        self.rt.block_on(prompts::create_version(&self.pool, v))
    }
    fn get_prompt_version(&self, prompt_id: &str, version: u32) -> Result<Option<PromptVersion>> {
        self.rt
            .block_on(prompts::get_version(&self.pool, prompt_id, version))
    }
    fn list_prompt_versions(&self, prompt_id: &str) -> Result<Vec<PromptVersion>> {
        self.rt
            .block_on(prompts::list_versions(&self.pool, prompt_id))
    }
}
