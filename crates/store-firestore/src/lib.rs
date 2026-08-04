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
    ApiKey, Benchmark, BenchmarkRun, CostByDimension, Dataset, DatasetItem, Job, JobCancel, LimitRule,
    LimitScope, LlmEvent, ModelPriceRow, Project, Prompt, PromptVersion, RevenueEvent, Rubric,
    Score, TraceSummary,
};
use lighttrack_store::{
    insert_event_checked_nonatomic, insert_events_checked_nonatomic, Admission, CostRow,
    EventFilter, EventPage, Result, ScopeUsage, Store, StoreError, TraceEvents, Usage,
    UseCaseCostRow,
};

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
        // Say it out loud, once, where an operator configuring caps will see it. Firestore has no
        // server-side aggregate this backend can evaluate and write inside one transaction (usage is
        // summed client-side from a document scan), so admission here is check-then-act. An advisory
        // cap that reads as enforced is the failure mode we refuse; an honest warning is not.
        eprintln!(
            "lighttrack-store-firestore: usage caps are ADVISORY on this backend — admission is not \
             atomic, so a concurrent burst can exceed a cap before it takes effect. Postgres \
             (LIGHTTRACK_DATABASE_URL=postgres://…) enforces caps atomically."
        );
        // Same rule for the trace surface: this backend has no server-side grouping by `trace_id`,
        // so `/v1/traces` refuses with 501 `unsupported` rather than serving an empty page that
        // reads like "you have no traces". SQLite and Postgres implement it.
        eprintln!(
            "lighttrack-store-firestore: the TRACE surface is NOT served on this backend —              /v1/traces, /v1/traces/:id and whole-trace scoring answer HTTP 501 `unsupported`.              Use SQLite or Postgres (LIGHTTRACK_DATABASE_URL=postgres://…) for traces."
        );
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
    /// Left `false` deliberately (see the warning in [`FirestoreStore::connect`]): admission here is
    /// check-then-act, so the conformance suite reports the leak instead of pretending it enforces.
    fn admission_is_atomic(&self) -> bool {
        false
    }
    /// Spelled out rather than inherited, so the non-atomic path is a visible choice in this backend
    /// rather than a trait default nobody remembered was there.
    fn insert_event_checked(&self, ev: &LlmEvent) -> Result<Admission> {
        insert_event_checked_nonatomic(self, ev)
    }
    fn insert_events_checked(&self, evs: &[LlmEvent]) -> Vec<Result<Admission>> {
        insert_events_checked_nonatomic(self, evs)
    }
    fn list_events(&self, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>> {
        events::list_events(&self.rest, project, limit)
    }
    /// Left `false` deliberately (see the warning in [`FirestoreStore::connect`]): rolling events up
    /// by `trace_id` needs a server-side grouping this REST data plane doesn't have. Declaring it
    /// makes the refusal a tested property — the conformance suite asserts every trace method
    /// answers [`StoreError::Unsupported`], so this can never decay into a silent empty page.
    fn serves_traces(&self) -> bool {
        false
    }
    /// Spelled out rather than inherited, so the refusal is a visible choice in this backend rather
    /// than a trait default nobody remembered was there.
    fn list_traces(&self, _project: Option<&str>, _limit: usize) -> Result<Vec<TraceSummary>> {
        Err(StoreError::Unsupported("traces"))
    }
    fn list_trace_events(
        &self,
        _project: Option<&str>,
        _trace_id: &str,
        _max_spans: usize,
    ) -> Result<TraceEvents> {
        Err(StoreError::Unsupported("traces"))
    }
    fn list_trace_scores(&self, _project: Option<&str>, _trace_id: &str) -> Result<Vec<Score>> {
        Err(StoreError::Unsupported("traces"))
    }
    fn list_events_filtered(
        &self,
        project: Option<&str>,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<EventPage> {
        events::list_events_filtered(&self.rest, project, filter, limit)
    }
    fn cost_summary(&self, project: Option<&str>) -> Result<Vec<CostRow>> {
        events::cost_summary(&self.rest, project)
    }
    fn cost_summary_windowed(
        &self,
        project: Option<&str>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<Vec<CostRow>> {
        events::cost_summary_windowed(&self.rest, project, since, until)
    }
    fn usecase_costs(
        &self,
        project: Option<&str>,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<UseCaseCostRow>> {
        events::usecase_costs(&self.rest, project, since)
    }
    fn usage_since(&self, project: &str, since: DateTime<Utc>) -> Result<Usage> {
        events::usage_since(&self.rest, project, since)
    }
    fn usage_since_scoped(
        &self,
        project: &str,
        since: DateTime<Utc>,
        scope: &LimitScope,
    ) -> Result<Usage> {
        events::usage_since_scoped(&self.rest, project, since, scope)
    }
    fn usage_by_scope(
        &self,
        project: &str,
        since: DateTime<Utc>,
        kind: &str,
    ) -> Result<Vec<ScopeUsage>> {
        events::usage_by_scope(&self.rest, project, since, kind)
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
    fn get_limit_rule(&self, id: &str) -> Result<Option<LimitRule>> {
        limits::get_limit_rule(&self.rest, id)
    }
    fn update_limit_rule(&self, r: &LimitRule) -> Result<bool> {
        limits::update_limit_rule(&self.rest, r)
    }
    fn delete_limit_rule(&self, id: &str) -> Result<bool> {
        limits::delete_limit_rule(&self.rest, id)
    }

    fn insert_score(&self, s: &Score) -> Result<()> {
        scores::insert_score(&self.rest, s)
    }
    fn list_scores(&self, project: Option<&str>, limit: usize) -> Result<Vec<Score>> {
        scores::list_scores(&self.rest, project, limit)
    }
    fn list_run_scores(
        &self,
        run_id: &str,
        project: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Score>> {
        scores::list_run_scores(&self.rest, run_id, project, limit)
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
    fn cancel_job(&self, id: &str) -> Result<Option<JobCancel>> {
        jobs::cancel_job(&self.rest, id)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The trace refusal is reachable without any Firestore at all — it must be a *refusal*, not an
    /// empty page, and every entry point must agree (`get_trace` composes `list_trace_events`).
    /// Needs no emulator, so it guards the property in ordinary CI too.
    #[test]
    fn the_trace_surface_refuses_rather_than_reading_empty() {
        let store = FirestoreStore::connect("firestore://demo").expect("connect");
        assert!(!store.serves_traces());
        assert!(matches!(store.list_traces(Some("p"), 10), Err(StoreError::Unsupported(_))));
        assert!(matches!(
            store.list_traces_filtered(Some("p"), &lighttrack_store::TraceFilter::default(), 10),
            Err(StoreError::Unsupported(_))
        ));
        assert!(matches!(
            store.list_trace_events(Some("p"), "t", 10),
            Err(StoreError::Unsupported(_))
        ));
        assert!(matches!(store.list_trace_scores(Some("p"), "t"), Err(StoreError::Unsupported(_))));
        assert!(matches!(store.get_trace(Some("p"), "t", 10), Err(StoreError::Unsupported(_))));
    }
}
