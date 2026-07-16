//! Firestore backend for LightTrack — implements [`lighttrack_store::Store`] over the Firestore REST
//! API (blocking `reqwest`, no gRPC). Selected by `LIGHTTRACK_DATABASE_URL=firestore://<project-id>`.
//!
//! Auth: the **emulator** (`FIRESTORE_EMULATOR_HOST`) needs no token — used for local/CI verification.
//! On GCP, a bearer token is read from `GOOGLE_OAUTH_TOKEN` (metadata-server/ADC wiring is a follow-up).
//!
//! Part 1 (this module): the core data plane — events (incl. client-side cost/usage aggregation),
//! projects, api_keys, scores, prices, limits. Benchmark/dataset/rubric/job methods are part 2.

mod benchmarks;
mod codec;
mod datasets;
mod events;
mod jobs;
mod limits;
mod prices;
mod projects;
mod prompts;
mod rest;
mod revenue;
mod rubrics;
mod scores;

use chrono::{DateTime, Utc};
use serde_json::Value;

use lighttrack_core::{
    ApiKey, Benchmark, BenchmarkRun, CostByDimension, Dataset, DatasetItem, Job, LimitRule, LlmEvent,
    ModelPriceRow, Project, Prompt, PromptVersion, RevenueEvent, Rubric, Score,
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
    fn list_api_keys(&self, project: &str) -> Result<Vec<ApiKey>> {
        projects::list_api_keys(&self.rest, project)
    }
    fn set_api_key_revoked(&self, id: &str, revoked: bool) -> Result<bool> {
        projects::set_api_key_revoked(&self.rest, id, revoked)
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
    fn scored_event_ids(&self, event_ids: &[String]) -> Result<Vec<String>> {
        scores::scored_event_ids(&self.rest, event_ids)
    }

    fn upsert_price(&self, p: &ModelPriceRow) -> Result<()> {
        prices::upsert_price(&self.rest, p)
    }
    fn list_prices(&self) -> Result<Vec<ModelPriceRow>> {
        prices::list_prices(&self.rest)
    }

    // ---- benchmarks / datasets / rubrics / jobs (part 2) -------------------
    fn create_benchmark(&self, b: &Benchmark) -> Result<()> {
        benchmarks::create_benchmark(&self.rest, b)
    }
    fn get_benchmark(&self, id: &str) -> Result<Option<Benchmark>> {
        benchmarks::get_benchmark(&self.rest, id)
    }
    fn list_benchmarks(&self, project: &str) -> Result<Vec<Benchmark>> {
        benchmarks::list_benchmarks(&self.rest, project)
    }
    fn create_benchmark_run(&self, r: &BenchmarkRun) -> Result<()> {
        benchmarks::create_benchmark_run(&self.rest, r)
    }
    fn list_benchmark_runs(&self, benchmark_id: &str) -> Result<Vec<BenchmarkRun>> {
        benchmarks::list_benchmark_runs(&self.rest, benchmark_id)
    }
    fn create_dataset(&self, d: &Dataset) -> Result<()> {
        datasets::create_dataset(&self.rest, d)
    }
    fn get_dataset(&self, id: &str) -> Result<Option<Dataset>> {
        datasets::get_dataset(&self.rest, id)
    }
    fn list_datasets(&self, project: &str) -> Result<Vec<Dataset>> {
        datasets::list_datasets(&self.rest, project)
    }
    fn set_dataset_frozen(&self, id: &str, frozen: bool) -> Result<()> {
        datasets::set_dataset_frozen(&self.rest, id, frozen)
    }
    fn create_dataset_item(&self, item: &DatasetItem) -> Result<()> {
        datasets::create_dataset_item(&self.rest, item)
    }
    fn list_dataset_items(&self, dataset_id: &str) -> Result<Vec<DatasetItem>> {
        datasets::list_dataset_items(&self.rest, dataset_id)
    }
    fn create_rubric(&self, r: &Rubric) -> Result<()> {
        rubrics::create_rubric(&self.rest, r)
    }
    fn get_rubric(&self, id: &str) -> Result<Option<Rubric>> {
        rubrics::get_rubric(&self.rest, id)
    }
    fn list_rubrics(&self, project: &str) -> Result<Vec<Rubric>> {
        rubrics::list_rubrics(&self.rest, project)
    }
    fn create_job(&self, j: &Job) -> Result<()> {
        jobs::create_job(&self.rest, j)
    }
    fn claim_job(&self, stale_before: DateTime<Utc>) -> Result<Option<Job>> {
        jobs::claim_job(&self.rest, stale_before)
    }
    fn update_job_progress(&self, id: &str, progress: &str) -> Result<()> {
        jobs::update_job_progress(&self.rest, id, progress)
    }
    fn finish_job(&self, id: &str, status: &str, result: &Value, error: Option<&str>) -> Result<()> {
        jobs::finish_job(&self.rest, id, status, result, error)
    }
    fn get_job(&self, id: &str) -> Result<Option<Job>> {
        jobs::get_job(&self.rest, id)
    }
    fn list_jobs(&self, status: Option<&str>, limit: usize) -> Result<Vec<Job>> {
        jobs::list_jobs(&self.rest, status, limit)
    }

    // ---- prompt registry ---------------------------------------------------
    fn create_prompt(&self, p: &Prompt) -> Result<()> {
        prompts::create_prompt(&self.rest, p)
    }
    fn update_prompt(&self, p: &Prompt) -> Result<()> {
        prompts::update_prompt(&self.rest, p)
    }
    fn get_prompt(&self, project: &str, name: &str) -> Result<Option<Prompt>> {
        prompts::get_prompt(&self.rest, project, name)
    }
    fn get_prompt_by_id(&self, id: &str) -> Result<Option<Prompt>> {
        prompts::get_prompt_by_id(&self.rest, id)
    }
    fn list_prompts(&self, project: &str) -> Result<Vec<Prompt>> {
        prompts::list_prompts(&self.rest, project)
    }
    fn create_prompt_version(&self, v: &PromptVersion) -> Result<()> {
        prompts::create_prompt_version(&self.rest, v)
    }
    fn get_prompt_version(&self, prompt_id: &str, version: u32) -> Result<Option<PromptVersion>> {
        prompts::get_prompt_version(&self.rest, prompt_id, version)
    }
    fn list_prompt_versions(&self, prompt_id: &str) -> Result<Vec<PromptVersion>> {
        prompts::list_prompt_versions(&self.rest, prompt_id)
    }

    // ---- revenue + margin (Phase 1 profit tracking) ------------------------
    // `insert_revenue_events` (batch) uses the trait default loop — Firestore REST has no
    // multi-document transaction here, matching the Postgres backend's choice.
    fn insert_revenue_event(&self, ev: &RevenueEvent) -> Result<()> {
        revenue::insert(&self.rest, ev)
    }
    fn list_revenue_events(
        &self,
        project: Option<&str>,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<RevenueEvent>> {
        revenue::list(&self.rest, project, since, until)
    }
    fn cost_by_dimension(
        &self,
        project: Option<&str>,
        dim: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<CostByDimension>> {
        revenue::cost_by_dimension(&self.rest, project, dim, since, until)
    }
}
