//! SQLite-backed [`Store`] — the local-development backend (bundled SQLite, no external service).
//!
//! `SqliteStore` holds a mutex-guarded connection; the `Store` impl locks it and delegates to a
//! per-domain submodule of free functions over `&Connection` (`events`, `scores`, `projects`,
//! `benchmarks`, `datasets`, `rubrics`, `prices`, `jobs`). The timestamp/enum/JSON codecs are shared
//! across all backends — see [`crate::codec`].

mod benchmarks;
mod collective;
mod datasets;
mod events;
mod forecast;
mod jobs;
mod limits;
mod prices;
mod projects;
mod prompts;
mod relay;
mod revenue;
mod rubrics;
mod scores;
mod usage_cache;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde_json::Value;

use lighttrack_core::{
    ApiKey, Benchmark, BenchmarkRun, CollectiveEntry, CostByDimension, Dataset, DatasetItem, Job,
    LimitRule, LimitScope, LlmEvent, ModelPriceRow, Project, Prompt, PromptVersion, RelayOutcome,
    RelayTask, RevenueEvent, Rubric, Score, TokensByDimension, TraceSummary,
};

use crate::{
    Admission, CostRow, CustomerCostRow, DailyDimCost, DailyUsage, EventFilter, EventPage, Result,
    Store, TraceFilter, TracePage, Usage, UseCaseCostRow,
};

const SCHEMA: &str = include_str!("../../../../schema/sqlite/001_init.sql");

/// SQLite store. A single connection guarded by a mutex — fine for our throughput (≤1k calls/hr).
pub struct SqliteStore {
    conn: Mutex<Connection>,
    /// Incremental rolling-usage totals for admission control, so a cap check costs `O(new events)`
    /// instead of re-aggregating the whole window. Locked *before* `conn` in the two admission
    /// methods, so the count-then-insert stays one atomic critical section. See [`usage_cache`].
    usage_cache: Mutex<usage_cache::UsageCache>,
}

impl SqliteStore {
    /// Open (creating parent dirs and the file if needed) and ensure the schema exists.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let store = Self {
            conn: Mutex::new(Connection::open(path)?),
            usage_cache: Mutex::new(usage_cache::UsageCache::default()),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// In-memory store, for tests.
    pub fn open_in_memory() -> Result<Self> {
        let store = Self {
            conn: Mutex::new(Connection::open_in_memory()?),
            usage_cache: Mutex::new(usage_cache::UsageCache::default()),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Run a closure with the locked connection.
    fn with<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        f(&self.conn.lock().unwrap())
    }
}

impl Store for SqliteStore {
    fn init_schema(&self) -> Result<()> {
        self.with(|c| {
            c.execute_batch(SCHEMA)?;
            // Additive migration for DBs created before `events.name` existed. There's no migration
            // runner here (the schema batch is CREATE ... IF NOT EXISTS, which skips existing tables),
            // so ALTER the column in and treat only "duplicate column name" (already applied) as
            // success — re-raise anything else.
            if let Err(e) = c.execute("ALTER TABLE events ADD COLUMN name TEXT", []) {
                if !e.to_string().contains("duplicate column name") {
                    return Err(e.into());
                }
            }
            // Additive migrations for limit rules created before the soft-warning tier and
            // dimension scoping existed. Each tolerates "duplicate column name" (already applied).
            for stmt in [
                "ALTER TABLE limit_rules ADD COLUMN warn_at REAL",
                "ALTER TABLE limit_rules ADD COLUMN scope_kind TEXT",
                "ALTER TABLE limit_rules ADD COLUMN scope_value TEXT",
                // Collective digest v2: per-bucket quality variance (for merged CIs), plus the coarse
                // judge family and rubric-shape fingerprint that scored the bucket.
                "ALTER TABLE collective_entries ADD COLUMN quality_variance REAL",
                "ALTER TABLE collective_entries ADD COLUMN judge_provider TEXT",
                "ALTER TABLE collective_entries ADD COLUMN rubric_fingerprint TEXT",
            ] {
                if let Err(e) = c.execute(stmt, []) {
                    if !e.to_string().contains("duplicate column name") {
                        return Err(e.into());
                    }
                }
            }
            Ok(())
        })
    }

    // --- events ---
    fn insert_event(&self, ev: &LlmEvent) -> Result<()> {
        self.with(|c| events::insert(c, ev))
    }
    fn insert_event_checked(&self, ev: &LlmEvent) -> Result<Admission> {
        // Lock the usage cache *before* the connection (consistent order in both admission methods,
        // so no deadlock) and hold both across the check-count-insert — one atomic critical section.
        let mut cache = self.usage_cache.lock().unwrap();
        self.with(|c| events::insert_checked(c, &mut cache, ev))
    }
    fn insert_events_checked(&self, evs: &[LlmEvent]) -> Vec<Result<Admission>> {
        // One critical section for the whole batch: the cache + connection locks are held across every
        // item, so each accepted insert is already visible to the next item's usage read (no cap
        // bypass), and the check-then-insert stays atomic against concurrent ingest.
        let mut cache = self.usage_cache.lock().unwrap();
        self.with(|c| evs.iter().map(|e| events::insert_checked(c, &mut cache, e)).collect())
    }
    fn list_events(&self, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>> {
        self.with(|c| events::list(c, project, limit))
    }
    fn list_events_filtered(
        &self,
        project: Option<&str>,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<EventPage> {
        self.with(|c| events::list_filtered(c, project, filter, limit))
    }
    fn cost_summary(&self, project: Option<&str>) -> Result<Vec<CostRow>> {
        self.with(|c| events::cost_summary(c, project))
    }
    fn cost_summary_windowed(
        &self,
        project: Option<&str>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<Vec<CostRow>> {
        self.with(|c| events::cost_summary_windowed(c, project, since, until))
    }
    fn usecase_costs(
        &self,
        project: Option<&str>,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<UseCaseCostRow>> {
        self.with(|c| events::usecase_costs(c, project, since))
    }
    fn usage_since(&self, project: &str, since: DateTime<Utc>) -> Result<Usage> {
        self.with(|c| events::usage_since(c, project, since))
    }
    fn usage_since_scoped(
        &self,
        project: &str,
        since: DateTime<Utc>,
        scope: &LimitScope,
    ) -> Result<Usage> {
        self.with(|c| events::usage_since_scoped(c, project, since, scope))
    }
    fn daily_usage(
        &self,
        project: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<DailyUsage>> {
        self.with(|c| forecast::daily_usage(c, project, since, until))
    }
    fn daily_cost_by_dimension(
        &self,
        project: Option<&str>,
        dim: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<DailyDimCost>> {
        self.with(|c| forecast::daily_cost_by_dimension(c, project, dim, since, until))
    }
    fn get_event(&self, id: &str) -> Result<Option<LlmEvent>> {
        self.with(|c| events::get(c, id))
    }

    // --- traces ---
    fn list_traces(&self, project: Option<&str>, limit: usize) -> Result<Vec<TraceSummary>> {
        self.with(|c| events::list_trace_summaries(c, project, limit))
    }
    fn list_traces_filtered(
        &self,
        project: Option<&str>,
        filter: &TraceFilter,
        limit: usize,
    ) -> Result<TracePage> {
        self.with(|c| events::list_trace_summaries_filtered(c, project, filter, limit))
    }
    fn list_trace_events(&self, trace_id: &str) -> Result<Vec<LlmEvent>> {
        self.with(|c| events::list_by_trace(c, trace_id))
    }
    fn list_trace_scores(&self, trace_id: &str) -> Result<Vec<Score>> {
        self.with(|c| scores::list_by_trace(c, trace_id))
    }

    // --- scores ---
    fn insert_score(&self, s: &Score) -> Result<()> {
        self.with(|c| scores::insert(c, s))
    }
    fn list_scores(&self, project: Option<&str>, limit: usize) -> Result<Vec<Score>> {
        self.with(|c| scores::list(c, project, limit))
    }

    // --- projects / api keys / limits ---
    fn create_project(&self, p: &Project) -> Result<()> {
        self.with(|c| projects::create(c, p))
    }
    fn get_project(&self, id: &str) -> Result<Option<Project>> {
        self.with(|c| projects::get(c, id))
    }
    fn list_projects(&self) -> Result<Vec<Project>> {
        self.with(projects::list)
    }
    fn create_api_key(&self, k: &ApiKey) -> Result<()> {
        self.with(|c| projects::create_key(c, k))
    }
    fn find_api_key_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>> {
        self.with(|c| projects::find_key_by_prefix(c, prefix))
    }
    fn touch_api_key(&self, id: &str, when: DateTime<Utc>) -> Result<()> {
        self.with(|c| projects::touch_key(c, id, when))
    }
    fn create_limit_rule(&self, r: &LimitRule) -> Result<()> {
        self.with(|c| limits::create(c, r))
    }
    fn list_limit_rules(&self, project: &str, only_enabled: bool) -> Result<Vec<LimitRule>> {
        self.with(|c| limits::list(c, project, only_enabled))
    }
    fn get_limit_rule(&self, id: &str) -> Result<Option<LimitRule>> {
        self.with(|c| limits::get(c, id))
    }
    fn update_limit_rule(&self, r: &LimitRule) -> Result<bool> {
        self.with(|c| limits::update(c, r))
    }
    fn delete_limit_rule(&self, id: &str) -> Result<bool> {
        self.with(|c| limits::delete(c, id))
    }

    // --- benchmarks ---
    fn create_benchmark(&self, b: &Benchmark) -> Result<()> {
        self.with(|c| benchmarks::create(c, b))
    }
    fn get_benchmark(&self, id: &str) -> Result<Option<Benchmark>> {
        self.with(|c| benchmarks::get(c, id))
    }
    fn list_benchmarks(&self, project: &str) -> Result<Vec<Benchmark>> {
        self.with(|c| benchmarks::list(c, project))
    }
    fn create_benchmark_run(&self, r: &BenchmarkRun) -> Result<()> {
        self.with(|c| benchmarks::create_run(c, r))
    }
    fn list_benchmark_runs(&self, benchmark_id: &str) -> Result<Vec<BenchmarkRun>> {
        self.with(|c| benchmarks::list_runs(c, benchmark_id))
    }

    // --- prices ---
    fn upsert_price(&self, p: &ModelPriceRow) -> Result<()> {
        self.with(|c| prices::upsert(c, p))
    }
    fn list_prices(&self) -> Result<Vec<ModelPriceRow>> {
        self.with(prices::list)
    }

    // --- datasets ---
    fn create_dataset(&self, d: &Dataset) -> Result<()> {
        self.with(|c| datasets::create(c, d))
    }
    fn get_dataset(&self, id: &str) -> Result<Option<Dataset>> {
        self.with(|c| datasets::get(c, id))
    }
    fn list_datasets(&self, project: &str) -> Result<Vec<Dataset>> {
        self.with(|c| datasets::list(c, project))
    }
    fn set_dataset_frozen(&self, id: &str, frozen: bool) -> Result<()> {
        self.with(|c| datasets::set_frozen(c, id, frozen))
    }
    fn create_dataset_item(&self, item: &DatasetItem) -> Result<()> {
        self.with(|c| datasets::create_item(c, item))
    }
    fn list_dataset_items(&self, dataset_id: &str) -> Result<Vec<DatasetItem>> {
        self.with(|c| datasets::list_items(c, dataset_id))
    }

    // --- rubrics ---
    fn create_rubric(&self, r: &Rubric) -> Result<()> {
        self.with(|c| rubrics::create(c, r))
    }
    fn get_rubric(&self, id: &str) -> Result<Option<Rubric>> {
        self.with(|c| rubrics::get(c, id))
    }
    fn list_rubrics(&self, project: &str) -> Result<Vec<Rubric>> {
        self.with(|c| rubrics::list(c, project))
    }

    // --- jobs ---
    fn create_job(&self, j: &Job) -> Result<()> {
        self.with(|c| jobs::create(c, j))
    }
    fn claim_job(&self, stale_before: DateTime<Utc>) -> Result<Option<Job>> {
        self.with(|c| jobs::claim(c, stale_before))
    }
    fn update_job_progress(&self, id: &str, progress: &str) -> Result<()> {
        self.with(|c| jobs::update_progress(c, id, progress))
    }
    fn finish_job(&self, id: &str, status: &str, result: &Value, error: Option<&str>) -> Result<()> {
        self.with(|c| jobs::finish(c, id, status, result, error))
    }
    fn get_job(&self, id: &str) -> Result<Option<Job>> {
        self.with(|c| jobs::get(c, id))
    }
    fn list_jobs(&self, status: Option<&str>, limit: usize) -> Result<Vec<Job>> {
        self.with(|c| jobs::list(c, status, limit))
    }

    // --- prompt registry ---
    fn create_prompt(&self, p: &Prompt) -> Result<()> {
        self.with(|c| prompts::create(c, p))
    }
    fn update_prompt(&self, p: &Prompt) -> Result<()> {
        self.with(|c| prompts::update(c, p))
    }
    fn get_prompt(&self, project: &str, name: &str) -> Result<Option<Prompt>> {
        self.with(|c| prompts::get(c, project, name))
    }
    fn get_prompt_by_id(&self, id: &str) -> Result<Option<Prompt>> {
        self.with(|c| prompts::get_by_id(c, id))
    }
    fn list_prompts(&self, project: &str) -> Result<Vec<Prompt>> {
        self.with(|c| prompts::list(c, project))
    }
    fn create_prompt_version(&self, v: &PromptVersion) -> Result<()> {
        self.with(|c| prompts::create_version(c, v))
    }
    fn get_prompt_version(&self, prompt_id: &str, version: u32) -> Result<Option<PromptVersion>> {
        self.with(|c| prompts::get_version(c, prompt_id, version))
    }
    fn list_prompt_versions(&self, prompt_id: &str) -> Result<Vec<PromptVersion>> {
        self.with(|c| prompts::list_versions(c, prompt_id))
    }

    // --- revenue + margin (Phase 1 profit tracking) ---
    fn insert_revenue_event(&self, ev: &RevenueEvent) -> Result<()> {
        self.with(|c| revenue::insert(c, ev))
    }
    fn insert_revenue_events(&self, evs: &[RevenueEvent]) -> Result<()> {
        self.with(|c| revenue::insert_batch(c, evs))
    }
    fn list_revenue_events(
        &self,
        project: Option<&str>,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<RevenueEvent>> {
        self.with(|c| revenue::list(c, project, since, until))
    }
    fn cost_by_dimension(
        &self,
        project: Option<&str>,
        dim: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<CostByDimension>> {
        self.with(|c| revenue::cost_by_dimension(c, project, dim, since, until))
    }
    fn tokens_by_dimension(
        &self,
        project: Option<&str>,
        dim: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<TokensByDimension>> {
        self.with(|c| revenue::tokens_by_dimension(c, project, dim, since, until))
    }
    fn customer_cost_by_model(
        &self,
        project: Option<&str>,
        customer: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<CustomerCostRow>> {
        self.with(|c| revenue::customer_cost_by_model(c, project, customer, since, until))
    }
    fn customer_cost_by_name(
        &self,
        project: Option<&str>,
        customer: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<CustomerCostRow>> {
        self.with(|c| revenue::customer_cost_by_name(c, project, customer, since, until))
    }

    // --- cloud→device relay queue ---
    fn create_relay_task(&self, t: &RelayTask) -> Result<()> {
        self.with(|c| relay::create(c, t))
    }
    fn get_relay_task(&self, id: &str) -> Result<Option<RelayTask>> {
        self.with(|c| relay::get(c, id))
    }
    fn find_relay_task_by_key(&self, project: &str, key: &str) -> Result<Option<RelayTask>> {
        self.with(|c| relay::find_by_key(c, project, key))
    }
    fn list_relay_tasks(
        &self,
        project: Option<&str>,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RelayTask>> {
        self.with(|c| relay::list(c, project, status, limit))
    }
    fn lease_relay_tasks(&self, device: &str, lease_secs: i64, max: usize) -> Result<Vec<RelayTask>> {
        self.with(|c| relay::lease(c, device, lease_secs, max))
    }
    fn sweep_relay_dead(&self) -> Result<Vec<RelayTask>> {
        self.with(relay::sweep_dead)
    }
    fn settle_relay_task(&self, id: &str, outcome: &RelayOutcome) -> Result<Option<RelayTask>> {
        self.with(|c| relay::settle(c, id, outcome))
    }

    // --- collective model intelligence ---
    fn upsert_collective_entry(&self, e: &CollectiveEntry) -> Result<()> {
        self.with(|c| collective::upsert(c, e))
    }
    fn delete_collective_entries(&self, contributor_id: &str) -> Result<u64> {
        self.with(|c| collective::delete(c, contributor_id))
    }
    fn list_collective_entries(&self) -> Result<Vec<CollectiveEntry>> {
        self.with(collective::list)
    }
}
