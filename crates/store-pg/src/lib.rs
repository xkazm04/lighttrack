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
//! `relay`), mirroring the SQLite backend's layout. `claim_job` and the relay `lease` use
//! `FOR UPDATE SKIP LOCKED … RETURNING` for concurrency-safe atomic dequeues.

mod benchmarks;
mod datasets;
mod events;
mod jobs;
mod prices;
mod projects;
mod relay;
mod revenue;
mod rubrics;
mod scores;
mod util;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::runtime::Runtime;

use lighttrack_core::{
    ApiKey, Benchmark, BenchmarkRun, CostByDimension, Dataset, DatasetItem, Job, LimitRule, LlmEvent,
    ModelPriceRow, Project, RelayOutcome, RelayTask, RevenueEvent, Rubric, Score,
};
use lighttrack_store::{CostRow, Result, Store, StoreError, Usage};

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

impl Store for PgStore {
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
    fn list_events(&self, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>> {
        self.rt.block_on(events::list(&self.pool, project, limit))
    }
    fn cost_summary(&self, project: Option<&str>) -> Result<Vec<CostRow>> {
        self.rt.block_on(events::cost_summary(&self.pool, project))
    }
    fn usage_since(&self, project: &str, since: DateTime<Utc>) -> Result<Usage> {
        self.rt.block_on(events::usage_since(&self.pool, project, since))
    }
    fn get_event(&self, id: &str) -> Result<Option<LlmEvent>> {
        self.rt.block_on(events::get(&self.pool, id))
    }

    // --- projects / api keys / limits ---
    fn create_project(&self, p: &Project) -> Result<()> {
        self.rt.block_on(projects::create(&self.pool, p))
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
        self.rt.block_on(projects::find_key_by_prefix(&self.pool, prefix))
    }
    fn touch_api_key(&self, id: &str, when: DateTime<Utc>) -> Result<()> {
        self.rt.block_on(projects::touch_key(&self.pool, id, when))
    }
    fn list_api_keys(&self, project: &str) -> Result<Vec<ApiKey>> {
        self.rt.block_on(projects::list_keys(&self.pool, project))
    }
    fn set_api_key_revoked(&self, id: &str, revoked: bool) -> Result<bool> {
        self.rt.block_on(projects::set_key_revoked(&self.pool, id, revoked))
    }
    fn create_limit_rule(&self, r: &LimitRule) -> Result<()> {
        self.rt.block_on(projects::create_limit(&self.pool, r))
    }
    fn list_limit_rules(&self, project: &str, only_enabled: bool) -> Result<Vec<LimitRule>> {
        self.rt.block_on(projects::list_limits(&self.pool, project, only_enabled))
    }

    // --- scores ---
    fn insert_score(&self, s: &Score) -> Result<()> {
        self.rt.block_on(scores::insert(&self.pool, s))
    }
    fn list_scores(&self, project: Option<&str>, limit: usize) -> Result<Vec<Score>> {
        self.rt.block_on(scores::list(&self.pool, project, limit))
    }
    fn scored_event_ids(&self, event_ids: &[String]) -> Result<Vec<String>> {
        self.rt.block_on(scores::scored_event_ids(&self.pool, event_ids))
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
        self.rt.block_on(benchmarks::list_runs(&self.pool, benchmark_id))
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
        self.rt.block_on(datasets::set_frozen(&self.pool, id, frozen))
    }
    fn create_dataset_item(&self, item: &DatasetItem) -> Result<()> {
        self.rt.block_on(datasets::create_item(&self.pool, item))
    }
    fn list_dataset_items(&self, dataset_id: &str) -> Result<Vec<DatasetItem>> {
        self.rt.block_on(datasets::list_items(&self.pool, dataset_id))
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
    fn claim_job(&self, stale_before: DateTime<Utc>) -> Result<Option<Job>> {
        self.rt.block_on(jobs::claim(&self.pool, stale_before))
    }
    fn update_job_progress(&self, id: &str, progress: &str) -> Result<()> {
        self.rt.block_on(jobs::update_progress(&self.pool, id, progress))
    }
    fn finish_job(&self, id: &str, status: &str, result: &Value, error: Option<&str>) -> Result<()> {
        self.rt.block_on(jobs::finish(&self.pool, id, status, result, error))
    }
    fn get_job(&self, id: &str) -> Result<Option<Job>> {
        self.rt.block_on(jobs::get(&self.pool, id))
    }
    fn list_jobs(&self, status: Option<&str>, limit: usize) -> Result<Vec<Job>> {
        self.rt.block_on(jobs::list(&self.pool, status, limit))
    }

    // --- cloud→device relay queue ---
    fn create_relay_task(&self, t: &RelayTask) -> Result<()> {
        self.rt.block_on(relay::create(&self.pool, t))
    }
    fn get_relay_task(&self, id: &str) -> Result<Option<RelayTask>> {
        self.rt.block_on(relay::get(&self.pool, id))
    }
    fn find_relay_task_by_key(&self, project: &str, key: &str) -> Result<Option<RelayTask>> {
        self.rt.block_on(relay::find_by_key(&self.pool, project, key))
    }
    fn list_relay_tasks(
        &self,
        project: Option<&str>,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RelayTask>> {
        self.rt.block_on(relay::list(&self.pool, project, status, limit))
    }
    fn lease_relay_tasks(&self, device: &str, lease_secs: i64, max: usize) -> Result<Vec<RelayTask>> {
        self.rt.block_on(relay::lease(&self.pool, device, lease_secs, max))
    }
    fn sweep_relay_dead(&self) -> Result<Vec<RelayTask>> {
        self.rt.block_on(relay::sweep_dead(&self.pool))
    }
    fn settle_relay_task(&self, id: &str, outcome: &RelayOutcome) -> Result<Option<RelayTask>> {
        self.rt.block_on(relay::settle(&self.pool, id, outcome))
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
        self.rt.block_on(revenue::list(&self.pool, project, since, until))
    }
    fn cost_by_dimension(
        &self,
        project: Option<&str>,
        dim: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<CostByDimension>> {
        self.rt.block_on(revenue::cost_by_dimension(&self.pool, project, dim, since, until))
    }
}
