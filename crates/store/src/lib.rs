//! LightTrack persistence layer.
//!
//! [`Store`] is the backend-agnostic interface used by `api` (and later `mcp`/`cli`). The local
//! implementation is [`sqlite::SqliteStore`]; cloud backends slot in behind the same trait, selected
//! by `LIGHTTRACK_DATABASE_URL`: `lighttrack-store-pg` (Postgres, the cross-cloud default) and
//! `lighttrack-store-firestore` (GCP-native). See `docs/PACKAGING.md`.
//!
//! Methods are synchronous (SQLite is blocking). Async callers wrap them in `spawn_blocking`.

pub mod conformance;
pub mod sqlite;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use lighttrack_core::{
    ApiKey, Benchmark, BenchmarkRun, Dataset, DatasetItem, Job, LimitRule, LlmEvent, ModelPriceRow,
    Project, Rubric, Score,
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

/// Aggregate usage for a project over a time window — used to evaluate limits.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Usage {
    pub cost_usd: f64,
    pub calls: i64,
    pub tokens: i64,
}

/// Backend-agnostic persistence interface.
pub trait Store: Send + Sync {
    /// Create tables if they don't exist.
    fn init_schema(&self) -> Result<()>;

    /// Persist one normalized event.
    fn insert_event(&self, ev: &LlmEvent) -> Result<()>;

    /// Most recent events, newest first, optionally filtered by project.
    fn list_events(&self, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>>;

    /// Cost/usage rollup grouped by project + provider + model, optionally filtered by project.
    fn cost_summary(&self, project: Option<&str>) -> Result<Vec<CostRow>>;

    /// Aggregate usage for one project since `since` (inclusive). Used by limit evaluation.
    fn usage_since(&self, project: &str, since: DateTime<Utc>) -> Result<Usage>;

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

    // --- single event lookup + scores (Phase 3) ---
    fn get_event(&self, id: &str) -> Result<Option<LlmEvent>>;
    fn insert_score(&self, s: &Score) -> Result<()>;
    fn list_scores(&self, project: Option<&str>, limit: usize) -> Result<Vec<Score>>;

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
}
