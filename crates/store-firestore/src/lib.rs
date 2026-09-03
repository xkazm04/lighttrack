//! Firestore backend for LightTrack — implements [`lighttrack_store::Store`] over the Firestore REST
//! API (blocking `reqwest`, no gRPC). Selected by `LIGHTTRACK_DATABASE_URL=firestore://<project-id>`.
//!
//! Auth: the **emulator** (`FIRESTORE_EMULATOR_HOST`) needs no token — used for local/CI verification.
//! On GCP, a bearer token is read from `GOOGLE_OAUTH_TOKEN` (metadata-server/ADC wiring is a follow-up).
//!
//! Part 1 (this module): the core data plane — events (incl. client-side cost/usage aggregation),
//! projects, api_keys, scores, prices, limits. Benchmark/dataset/rubric/job methods are part 2.

mod alert_channels;
mod alerts;
mod benchmarks;
mod codec;
mod collective;
mod contributions;
mod datasets;
mod events;
mod jobs;
mod labels;
mod limits;
mod margin_policies;
mod price_fill;
mod prices;
mod projects;
mod prompts;
mod rest;
mod revenue;
mod rollup;
mod rubrics;
mod scope;
mod scores;

use chrono::{DateTime, Utc};
use serde_json::Value;

use lighttrack_core::{
    Alert, AlertChannel, ApiKey, Benchmark, BenchmarkRun, CalibrationRecord, CollectiveEntry,
    ContributionRecord, CostByDimension, Dataset, DatasetItem, Delivery, Job, JobCancel, JobFinish,
    Label, LabelFilter, LimitRule, LimitScope, LlmEvent, ModelPriceRow, Project, Prompt,
    PromptVersion, RevenueEvent, RollupQuery, RollupRow, Rubric, Score, TraceSummary,
};
use lighttrack_store::{
    capabilities::{Capabilities, Surface},
    insert_event_checked_nonatomic, insert_events_checked_nonatomic, Admission, AlertAdmission,
    AlertFilter, CollectiveFilter, CostRow, EventFilter, EventPage, ReplaceAck, Result, ScopeUsage,
    Store, StoreError, TraceEvents, Usage, UseCaseCostRow,
};

use lighttrack_store::Scope;
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
                std::env::var("GOOGLE_OAUTH_TOKEN")
                    .ok()
                    .filter(|s| !s.is_empty()),
            ),
        };
        let base = format!("{host}/v1/projects/{project}/databases/(default)/documents");
        Ok(Self {
            rest: Rest::new(base, token),
        })
    }
}

impl FirestoreStore {
    /// What this backend implements today, read off the `impl Store` block below.
    ///
    /// Two absences are properties of the data plane rather than unfinished work: `Traces` needs a
    /// server-side grouping by `trace_id` that the REST API has no equivalent for, and admission is
    /// therefore check-then-act (`atomic_admission: false` — usage is summed client-side from a
    /// document scan, so a concurrent burst can exceed a cap before it takes effect; caps here are
    /// ADVISORY, and Postgres is the backend that enforces them atomically). `Relay`, `Forecast`,
    /// `MarginBreakdowns` and `ProjectAdmin` are simply not ported. All of them refuse with
    /// `Unsupported` (HTTP 501) and the conformance suite asserts that refusal.
    ///
    /// `Rollup` — and, through the trait defaults over it, `Forecast` and `MarginBreakdowns` — is a
    /// client-side fold over the same windowed document scan every other aggregate here runs. One
    /// caveat rides with it and is documented in `rollup.rs`: Firestore stores no `received_at`, so
    /// an accounting window asked for on server-arrival time is answered on the client's `ts`.
    ///
    /// `Collective` is served, with one bounded caveat this backend states rather than hides: a
    /// contributor replacement larger than one `:commit` batch is chunked and reports
    /// `ReplaceAck::atomic == false` (see [`collective`]).
    pub const SURFACES: &'static [Surface] = &[
        Surface::EventsCore,
        Surface::EventFilters,
        Surface::Rollup,
        Surface::Forecast,
        Surface::MarginBreakdowns,
        Surface::Prompts,
        Surface::KeyAdmin,
        Surface::LimitLifecycle,
        Surface::MarginPolicies,
        Surface::JobLeases,
        Surface::Collective,
        Surface::Pricing,
        // The ledger and its routing table. Declared rather than inherited as `Unsupported`
        // for the reason the whole manifest exists: a Firestore deployment answering `[]` to
        // `GET /v1/alerts` would tell an operator nothing has ever fired here.
        Surface::Alerts,
        Surface::AlertRouting,
        // The contributor-side ledger, with its own stated caveat: the page and the per-hub probe
        // are ordered client-side, so that a fresh project needs no hand-declared composite index
        // (see [`contributions`]).
        Surface::Contributions,
        // The human verdict ledger and its calibration history. Declared rather than inherited as
        // `Unsupported` for the reason the whole manifest exists: an empty `GET /v1/labels` here
        // would tell an operator nobody has ever graded anything in this project — and a
        // calibration built on that measures a judge against nothing and calls it a regression.
        Surface::Labels,
        Surface::Calibrations,
    ];

    /// This backend's manifest as a pure function of the type — `lighttrack-store`'s parity-doc
    /// test renders the matrix from it without a live Firestore.
    pub fn manifest() -> Capabilities {
        Capabilities::new("firestore", Self::SURFACES, false)
    }
}

impl Store for FirestoreStore {
    fn capabilities(&self) -> Capabilities {
        Self::manifest()
    }

    // Firestore is schemaless — collections are created on first write.
    fn init_schema(&self) -> Result<()> {
        Ok(())
    }

    fn insert_event(&self, ev: &LlmEvent) -> Result<()> {
        events::insert_event(&self.rest, ev)
    }
    /// Spelled out rather than inherited, so the non-atomic path is a visible choice in this backend
    /// rather than a trait default nobody remembered was there.
    fn insert_event_checked(&self, ev: &LlmEvent) -> Result<Admission> {
        insert_event_checked_nonatomic(self, ev)
    }
    fn insert_events_checked(&self, evs: &[LlmEvent]) -> Vec<Result<Admission>> {
        insert_events_checked_nonatomic(self, evs)
    }
    fn list_events(&self, project: Scope<'_>, limit: usize) -> Result<Vec<LlmEvent>> {
        let project = project.project();
        events::list_events(&self.rest, project, limit)
    }
    /// Spelled out rather than inherited, so the refusal is a visible choice in this backend rather
    /// than a trait default nobody remembered was there. `Surface::Traces` is absent from
    /// [`FirestoreStore::SURFACES`], which is what makes the refusal a *tested* property.
    fn list_traces(&self, _project: Scope<'_>, _limit: usize) -> Result<Vec<TraceSummary>> {
        Err(StoreError::Unsupported("traces"))
    }
    fn list_trace_events(
        &self,
        _project: Scope<'_>,
        _trace_id: &str,
        _max_spans: usize,
    ) -> Result<TraceEvents> {
        Err(StoreError::Unsupported("traces"))
    }
    fn list_trace_scores(&self, _project: Scope<'_>, _trace_id: &str) -> Result<Vec<Score>> {
        Err(StoreError::Unsupported("traces"))
    }
    fn list_events_filtered(
        &self,
        project: Scope<'_>,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<EventPage> {
        let project = project.project();
        events::list_events_filtered(&self.rest, project, filter, limit)
    }
    fn cost_summary(&self, project: Scope<'_>) -> Result<Vec<CostRow>> {
        let project = project.project();
        events::cost_summary(&self.rest, project)
    }
    /// The grouped-rollup primitive — a client-side fold; the forecast and margin-breakdown surfaces
    /// reach this backend through the trait defaults over it.
    fn rollup(&self, q: &RollupQuery<'_>) -> Result<Vec<RollupRow>> {
        rollup::rollup(&self.rest, q)
    }
    fn cost_summary_windowed(
        &self,
        project: Scope<'_>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<Vec<CostRow>> {
        let project = project.project();
        events::cost_summary_windowed(&self.rest, project, since, until)
    }
    fn usecase_costs(
        &self,
        project: Scope<'_>,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<UseCaseCostRow>> {
        let project = project.project();
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
    fn get_event(&self, scope: Scope<'_>, id: &str) -> Result<Option<LlmEvent>> {
        events::get_event(&self.rest, scope.project(), id)
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
    fn set_api_key_expiry(&self, id: &str, when: Option<DateTime<Utc>>) -> Result<bool> {
        projects::set_api_key_expiry(&self.rest, id, when)
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
    fn get_limit_rule(&self, scope: Scope<'_>, id: &str) -> Result<Option<LimitRule>> {
        limits::get_limit_rule(&self.rest, scope.project(), id)
    }
    fn update_limit_rule(&self, scope: Scope<'_>, r: &LimitRule) -> Result<bool> {
        limits::update_limit_rule(&self.rest, scope.project(), r)
    }
    fn delete_limit_rule(&self, scope: Scope<'_>, id: &str) -> Result<bool> {
        limits::delete_limit_rule(&self.rest, scope.project(), id)
    }

    // --- margin policies ---
    fn create_margin_policy(&self, p: &lighttrack_core::MarginPolicy) -> Result<()> {
        margin_policies::create_margin_policy(&self.rest, p)
    }
    fn list_margin_policies(
        &self,
        project: &str,
        only_enabled: bool,
    ) -> Result<Vec<lighttrack_core::MarginPolicy>> {
        margin_policies::list_margin_policies(&self.rest, project, only_enabled)
    }
    fn get_margin_policy(
        &self,
        scope: Scope<'_>,
        id: &str,
    ) -> Result<Option<lighttrack_core::MarginPolicy>> {
        margin_policies::get_margin_policy(&self.rest, scope.project(), id)
    }
    fn delete_margin_policy(&self, scope: Scope<'_>, id: &str) -> Result<bool> {
        margin_policies::delete_margin_policy(&self.rest, scope.project(), id)
    }

    fn insert_score(&self, s: &Score) -> Result<()> {
        scores::insert_score(&self.rest, s)
    }
    fn list_scores(&self, project: Scope<'_>, limit: usize) -> Result<Vec<Score>> {
        let project = project.project();
        scores::list_scores(&self.rest, project, limit)
    }
    fn list_run_scores(
        &self,
        run_id: &str,
        project: Scope<'_>,
        limit: usize,
    ) -> Result<Vec<Score>> {
        let project = project.project();
        scores::list_run_scores(&self.rest, run_id, project, limit)
    }
    fn scored_event_ids(&self, scope: Scope<'_>, event_ids: &[String]) -> Result<Vec<String>> {
        scores::scored_event_ids(&self.rest, scope.project(), event_ids)
    }

    fn upsert_price(&self, p: &ModelPriceRow) -> Result<()> {
        prices::upsert_price(&self.rest, p)
    }
    fn list_prices(&self) -> Result<Vec<ModelPriceRow>> {
        prices::list_prices(&self.rest)
    }
    fn list_price_history(&self, provider: &str, model: &str) -> Result<Vec<ModelPriceRow>> {
        prices::history(&self.rest, provider, model)
    }
    fn fill_unpriced_cost(&self, f: &lighttrack_store::pricing::PriceFill<'_>) -> Result<u64> {
        price_fill::fill(&self.rest, f)
    }

    // ---- benchmarks / datasets / rubrics / jobs (part 2) -------------------
    fn create_benchmark(&self, b: &Benchmark) -> Result<()> {
        benchmarks::create_benchmark(&self.rest, b)
    }
    fn get_benchmark(&self, scope: Scope<'_>, id: &str) -> Result<Option<Benchmark>> {
        benchmarks::get_benchmark(&self.rest, scope.project(), id)
    }
    fn list_benchmarks(&self, project: &str) -> Result<Vec<Benchmark>> {
        benchmarks::list_benchmarks(&self.rest, project)
    }
    fn create_benchmark_run(&self, r: &BenchmarkRun) -> Result<()> {
        benchmarks::create_benchmark_run(&self.rest, r)
    }
    fn list_benchmark_runs(
        &self,
        scope: Scope<'_>,
        benchmark_id: &str,
    ) -> Result<Vec<BenchmarkRun>> {
        benchmarks::list_benchmark_runs(&self.rest, scope.project(), benchmark_id)
    }
    fn create_dataset(&self, d: &Dataset) -> Result<()> {
        datasets::create_dataset(&self.rest, d)
    }
    fn get_dataset(&self, scope: Scope<'_>, id: &str) -> Result<Option<Dataset>> {
        datasets::get_dataset(&self.rest, scope.project(), id)
    }
    fn list_datasets(&self, project: Scope<'_>) -> Result<Vec<Dataset>> {
        datasets::list_datasets(&self.rest, project.project())
    }
    fn set_dataset_frozen(&self, scope: Scope<'_>, id: &str, frozen: bool) -> Result<()> {
        datasets::set_dataset_frozen(&self.rest, scope.project(), id, frozen)
    }
    fn create_dataset_item(&self, item: &DatasetItem) -> Result<()> {
        datasets::create_dataset_item(&self.rest, item)
    }
    fn list_dataset_items(&self, scope: Scope<'_>, dataset_id: &str) -> Result<Vec<DatasetItem>> {
        datasets::list_dataset_items(&self.rest, scope.project(), dataset_id)
    }
    fn create_rubric(&self, r: &Rubric) -> Result<()> {
        rubrics::create_rubric(&self.rest, r)
    }
    fn get_rubric(&self, scope: Scope<'_>, id: &str) -> Result<Option<Rubric>> {
        rubrics::get_rubric(&self.rest, scope.project(), id)
    }
    fn list_rubrics(&self, project: &str) -> Result<Vec<Rubric>> {
        rubrics::list_rubrics(&self.rest, project)
    }
    fn create_job(&self, j: &Job) -> Result<()> {
        jobs::create_job(&self.rest, j)
    }
    fn claim_job(&self, stale_before: DateTime<Utc>, kinds: &[&str]) -> Result<Option<Job>> {
        jobs::claim_job(&self.rest, stale_before, kinds)
    }
    fn cancel_job(&self, scope: Scope<'_>, id: &str) -> Result<Option<JobCancel>> {
        jobs::cancel_job(&self.rest, scope.project(), id)
    }
    fn update_job_progress(&self, id: &str, progress: &str) -> Result<()> {
        jobs::update_job_progress(&self.rest, id, progress)
    }
    fn renew_job_lease(&self, id: &str, fence: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        jobs::renew_job_lease(&self.rest, id, fence)
    }
    fn finish_job(
        &self,
        id: &str,
        status: &str,
        result: &Value,
        error: Option<&str>,
        fence: Option<DateTime<Utc>>,
    ) -> Result<JobFinish> {
        jobs::finish_job(&self.rest, id, status, result, error, fence)
    }
    fn get_job(&self, scope: Scope<'_>, id: &str) -> Result<Option<Job>> {
        jobs::get_job(&self.rest, scope.project(), id)
    }
    fn list_jobs(&self, scope: Scope<'_>, status: Option<&str>, limit: usize) -> Result<Vec<Job>> {
        jobs::list_jobs(&self.rest, scope.project(), status, limit)
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
    fn get_prompt_by_id(&self, scope: Scope<'_>, id: &str) -> Result<Option<Prompt>> {
        prompts::get_prompt_by_id(&self.rest, scope.project(), id)
    }
    fn list_prompts(&self, project: &str) -> Result<Vec<Prompt>> {
        prompts::list_prompts(&self.rest, project)
    }
    fn create_prompt_version(&self, v: &PromptVersion) -> Result<()> {
        prompts::create_prompt_version(&self.rest, v)
    }
    fn get_prompt_version(
        &self,
        scope: Scope<'_>,
        prompt_id: &str,
        version: u32,
    ) -> Result<Option<PromptVersion>> {
        prompts::get_prompt_version(&self.rest, scope.project(), prompt_id, version)
    }
    fn list_prompt_versions(
        &self,
        scope: Scope<'_>,
        prompt_id: &str,
    ) -> Result<Vec<PromptVersion>> {
        prompts::list_prompt_versions(&self.rest, scope.project(), prompt_id)
    }

    // ---- revenue + margin (Phase 1 profit tracking) ------------------------
    // `insert_revenue_events` (batch) uses the trait default loop — Firestore REST has no
    // multi-document transaction here, matching the Postgres backend's choice.
    fn insert_revenue_event(&self, ev: &RevenueEvent) -> Result<()> {
        revenue::insert(&self.rest, ev)
    }
    fn list_revenue_events(
        &self,
        project: Scope<'_>,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<RevenueEvent>> {
        let project = project.project();
        revenue::list(&self.rest, project, since, until)
    }
    fn cost_by_dimension(
        &self,
        project: Scope<'_>,
        dim: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<CostByDimension>> {
        let project = project.project();
        revenue::cost_by_dimension(&self.rest, project, dim, since, until)
    }

    // --- collective model intelligence (the shared leaderboard) ---
    fn upsert_collective_entry(&self, e: &CollectiveEntry) -> Result<()> {
        collective::upsert(&self.rest, e)
    }
    fn delete_collective_entries(&self, contributor_id: &str) -> Result<u64> {
        collective::delete(&self.rest, contributor_id)
    }
    fn list_collective_entries(&self) -> Result<Vec<CollectiveEntry>> {
        collective::list(&self.rest)
    }
    fn purge_collective_entries_before(&self, cutoff: DateTime<Utc>) -> Result<u64> {
        collective::purge_before(&self.rest, cutoff)
    }
    fn replace_collective_contribution(
        &self,
        contributor_id: &str,
        entries: &[CollectiveEntry],
        purge_before: Option<DateTime<Utc>>,
    ) -> Result<ReplaceAck> {
        collective::replace(&self.rest, contributor_id, entries, purge_before)
    }
    fn latest_collective_receipt(&self, contributor_id: &str) -> Result<Option<DateTime<Utc>>> {
        collective::latest_receipt(&self.rest, contributor_id)
    }
    fn list_collective_entries_filtered(
        &self,
        f: &CollectiveFilter,
    ) -> Result<Vec<CollectiveEntry>> {
        collective::list_filtered(&self.rest, f)
    }

    // --- the contributor-side contribution ledger (M22) ---
    fn insert_contribution(&self, c: &ContributionRecord) -> Result<()> {
        contributions::insert(&self.rest, c)
    }
    fn list_contributions(
        &self,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<Vec<ContributionRecord>> {
        contributions::list(&self.rest, limit, cursor)
    }
    fn latest_contribution(&self, hub_url_hash: &str) -> Result<Option<ContributionRecord>> {
        contributions::latest(&self.rest, hub_url_hash)
    }

    // --- alert ledger + routing (M3) ---
    fn insert_alert_dedup(
        &self,
        a: &Alert,
        cooldown: std::time::Duration,
    ) -> Result<AlertAdmission> {
        alerts::insert_alert_dedup(&self.rest, a, cooldown)
    }
    fn mark_delivery(&self, alert_id: &str, d: &Delivery) -> Result<bool> {
        alerts::mark_delivery(&self.rest, alert_id, d)
    }
    fn list_alerts(&self, f: &AlertFilter) -> Result<Vec<Alert>> {
        alerts::list_alerts(&self.rest, f)
    }
    fn get_alert(&self, scope: Scope<'_>, id: &str) -> Result<Option<Alert>> {
        alerts::get_alert(&self.rest, scope.project(), id)
    }
    fn ack_alert(&self, scope: Scope<'_>, id: &str, by: &str, at: DateTime<Utc>) -> Result<bool> {
        alerts::ack_alert(&self.rest, scope.project(), id, by, at)
    }
    fn attach_alert_resolution(
        &self,
        scope: Scope<'_>,
        id: &str,
        resolution: &Value,
    ) -> Result<bool> {
        alerts::attach_alert_resolution(&self.rest, scope.project(), id, resolution)
    }

    fn create_alert_channel(&self, c: &AlertChannel) -> Result<()> {
        alert_channels::create_alert_channel(&self.rest, c)
    }
    fn get_alert_channel(&self, scope: Scope<'_>, id: &str) -> Result<Option<AlertChannel>> {
        alert_channels::get_alert_channel(&self.rest, scope.project(), id)
    }
    fn list_alert_channels(&self, project: Scope<'_>) -> Result<Vec<AlertChannel>> {
        let project = project.project();
        alert_channels::list_alert_channels(&self.rest, project)
    }
    fn delete_alert_channel(&self, scope: Scope<'_>, id: &str) -> Result<bool> {
        alert_channels::delete_alert_channel(&self.rest, scope.project(), id)
    }

    // --- the human verdict ledger + calibration history (M11) ---
    fn insert_label(&self, l: &Label) -> Result<()> {
        labels::insert_label(&self.rest, l)
    }
    fn list_labels(&self, f: &LabelFilter) -> Result<Vec<Label>> {
        labels::list_labels(&self.rest, f)
    }
    fn labels_for_dataset(&self, scope: Scope<'_>, dataset_id: &str) -> Result<Vec<Label>> {
        labels::labels_for_dataset(&self.rest, scope.project(), dataset_id)
    }
    fn insert_calibration(&self, c: &CalibrationRecord) -> Result<()> {
        labels::insert_calibration(&self.rest, c)
    }
    fn latest_calibration(
        &self,
        project: &str,
        rubric_id: Option<&str>,
        judge: &str,
    ) -> Result<Option<CalibrationRecord>> {
        labels::latest_calibration(&self.rest, project, rubric_id, judge)
    }
    fn list_calibrations(
        &self,
        project: Scope<'_>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<Vec<CalibrationRecord>> {
        let project = project.project();
        labels::list_calibrations(&self.rest, project, limit, cursor)
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
        assert!(matches!(
            store.list_traces(Scope::Project("p"), 10),
            Err(StoreError::Unsupported(_))
        ));
        assert!(matches!(
            store.list_traces_filtered(
                Scope::Project("p"),
                &lighttrack_store::TraceFilter::default(),
                10
            ),
            Err(StoreError::Unsupported(_))
        ));
        assert!(matches!(
            store.list_trace_events(Scope::Project("p"), "t", 10),
            Err(StoreError::Unsupported(_))
        ));
        assert!(matches!(
            store.list_trace_scores(Scope::Project("p"), "t"),
            Err(StoreError::Unsupported(_))
        ));
        assert!(matches!(
            store.get_trace(Scope::Project("p"), "t", 10),
            Err(StoreError::Unsupported(_))
        ));
    }
}
