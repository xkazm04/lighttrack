//! Firestore backend for LightTrack — implements [`lighttrack_store::Store`] over the Firestore REST
//! API (blocking `reqwest`, no gRPC). Selected by `LIGHTTRACK_DATABASE_URL=firestore://<project-id>`.
//!
//! Auth: the **emulator** (`FIRESTORE_EMULATOR_HOST`) needs no token — used for local/CI verification.
//! On GCP, a bearer token is read from `GOOGLE_OAUTH_TOKEN` (metadata-server/ADC wiring is a follow-up).
//!
//! Part 1 (this module): the core data plane — events (incl. client-side cost/usage aggregation),
//! projects, api_keys, scores, prices, limits. Benchmark/dataset/rubric/job methods are part 2.

mod codec;
mod events;
mod limits;
mod prices;
mod projects;
mod rest;
mod scores;

use chrono::{DateTime, Utc};
use serde_json::Value;

use lighttrack_core::{
    ApiKey, Benchmark, BenchmarkRun, Dataset, DatasetItem, Job, LimitRule, LlmEvent, ModelPriceRow,
    Project, Rubric, Score,
};
use lighttrack_store::{CostRow, Result, Store, StoreError, Usage};

use rest::Rest;

/// Firestore-backed [`Store`].
pub struct FirestoreStore {
    rest: Rest,
}

impl FirestoreStore {
    /// Connect from a `firestore://<project-id>` URL. Hits the emulator when `FIRESTORE_EMULATOR_HOST`
    /// is set, else `firestore.googleapis.com` with a `GOOGLE_OAUTH_TOKEN` bearer (if provided).
    pub fn connect(database_url: &str) -> Result<Self> {
        let project = database_url
            .strip_prefix("firestore://")
            .unwrap_or(database_url)
            .trim_matches('/');
        if project.is_empty() {
            return Err(StoreError::Other(
                "firestore url needs a project: firestore://<project-id>".into(),
            ));
        }
        let (host, token) = match std::env::var("FIRESTORE_EMULATOR_HOST") {
            Ok(h) if !h.trim().is_empty() => (format!("http://{}", h.trim()), None),
            _ => (
                "https://firestore.googleapis.com".to_string(),
                std::env::var("GOOGLE_OAUTH_TOKEN").ok().filter(|s| !s.is_empty()),
            ),
        };
        let base = format!("{host}/v1/projects/{project}/databases/(default)/documents");
        Ok(Self {
            rest: Rest::new(base, token),
        })
    }
}

fn nyi(method: &str) -> StoreError {
    StoreError::Other(format!(
        "firestore backend: `{method}` not yet implemented (Phase 5 / Firestore part 2)"
    ))
}

impl Store for FirestoreStore {
    // Firestore is schemaless — collections are created on first write.
    fn init_schema(&self) -> Result<()> {
        Ok(())
    }

    fn insert_event(&self, ev: &LlmEvent) -> Result<()> {
        events::insert_event(&self.rest, ev)
    }
    fn list_events(&self, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>> {
        events::list_events(&self.rest, project, limit)
    }
    fn cost_summary(&self, project: Option<&str>) -> Result<Vec<CostRow>> {
        events::cost_summary(&self.rest, project)
    }
    fn usage_since(&self, project: &str, since: DateTime<Utc>) -> Result<Usage> {
        events::usage_since(&self.rest, project, since)
    }
    fn get_event(&self, id: &str) -> Result<Option<LlmEvent>> {
        events::get_event(&self.rest, id)
    }

    fn create_project(&self, p: &Project) -> Result<()> {
        projects::create_project(&self.rest, p)
    }
    fn get_project(&self, id: &str) -> Result<Option<Project>> {
        projects::get_project(&self.rest, id)
    }
    fn list_projects(&self) -> Result<Vec<Project>> {
        projects::list_projects(&self.rest)
    }
    fn create_api_key(&self, k: &ApiKey) -> Result<()> {
        projects::create_api_key(&self.rest, k)
    }
    fn find_api_key_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>> {
        projects::find_api_key_by_prefix(&self.rest, prefix)
    }
    fn touch_api_key(&self, id: &str, when: DateTime<Utc>) -> Result<()> {
        projects::touch_api_key(&self.rest, id, when)
    }

    fn create_limit_rule(&self, r: &LimitRule) -> Result<()> {
        limits::create_limit_rule(&self.rest, r)
    }
    fn list_limit_rules(&self, project: &str, only_enabled: bool) -> Result<Vec<LimitRule>> {
        limits::list_limit_rules(&self.rest, project, only_enabled)
    }

    fn insert_score(&self, s: &Score) -> Result<()> {
        scores::insert_score(&self.rest, s)
    }
    fn list_scores(&self, project: Option<&str>, limit: usize) -> Result<Vec<Score>> {
        scores::list_scores(&self.rest, project, limit)
    }

    fn upsert_price(&self, p: &ModelPriceRow) -> Result<()> {
        prices::upsert_price(&self.rest, p)
    }
    fn list_prices(&self) -> Result<Vec<ModelPriceRow>> {
        prices::list_prices(&self.rest)
    }

    // ---- part 2 (benchmarks / datasets / rubrics / jobs) -------------------
    fn create_benchmark(&self, _b: &Benchmark) -> Result<()> {
        Err(nyi("create_benchmark"))
    }
    fn get_benchmark(&self, _id: &str) -> Result<Option<Benchmark>> {
        Err(nyi("get_benchmark"))
    }
    fn list_benchmarks(&self, _project: &str) -> Result<Vec<Benchmark>> {
        Err(nyi("list_benchmarks"))
    }
    fn create_benchmark_run(&self, _r: &BenchmarkRun) -> Result<()> {
        Err(nyi("create_benchmark_run"))
    }
    fn list_benchmark_runs(&self, _benchmark_id: &str) -> Result<Vec<BenchmarkRun>> {
        Err(nyi("list_benchmark_runs"))
    }
    fn create_dataset(&self, _d: &Dataset) -> Result<()> {
        Err(nyi("create_dataset"))
    }
    fn get_dataset(&self, _id: &str) -> Result<Option<Dataset>> {
        Err(nyi("get_dataset"))
    }
    fn list_datasets(&self, _project: &str) -> Result<Vec<Dataset>> {
        Err(nyi("list_datasets"))
    }
    fn set_dataset_frozen(&self, _id: &str, _frozen: bool) -> Result<()> {
        Err(nyi("set_dataset_frozen"))
    }
    fn create_dataset_item(&self, _item: &DatasetItem) -> Result<()> {
        Err(nyi("create_dataset_item"))
    }
    fn list_dataset_items(&self, _dataset_id: &str) -> Result<Vec<DatasetItem>> {
        Err(nyi("list_dataset_items"))
    }
    fn create_rubric(&self, _r: &Rubric) -> Result<()> {
        Err(nyi("create_rubric"))
    }
    fn get_rubric(&self, _id: &str) -> Result<Option<Rubric>> {
        Err(nyi("get_rubric"))
    }
    fn list_rubrics(&self, _project: &str) -> Result<Vec<Rubric>> {
        Err(nyi("list_rubrics"))
    }
    fn create_job(&self, _j: &Job) -> Result<()> {
        Err(nyi("create_job"))
    }
    fn claim_job(&self, _stale_before: DateTime<Utc>) -> Result<Option<Job>> {
        Err(nyi("claim_job"))
    }
    fn update_job_progress(&self, _id: &str, _progress: &str) -> Result<()> {
        Err(nyi("update_job_progress"))
    }
    fn finish_job(&self, _id: &str, _status: &str, _result: &Value, _error: Option<&str>) -> Result<()> {
        Err(nyi("finish_job"))
    }
    fn get_job(&self, _id: &str) -> Result<Option<Job>> {
        Err(nyi("get_job"))
    }
    fn list_jobs(&self, _status: Option<&str>, _limit: usize) -> Result<Vec<Job>> {
        Err(nyi("list_jobs"))
    }
}
