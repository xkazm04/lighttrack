//! The measurable for the liveness/readiness split, run against the wired router.
//!
//! The instrument is a store that can be **severed** mid-test: a real `SqliteStore` behind a
//! wrapper that starts answering `StoreError::Other` on every call the moment the flag flips. That
//! is the condition the split exists for — the process is fine, the dependency is not — and it is
//! the one condition a constant `/health` handler is structurally unable to report.
//!
//! Both numbers are measured here, in one run, over the same severed state:
//!
//! * **before** — `legacy_health`, the pre-split handler reproduced verbatim, mounted on a scratch
//!   router. It is what the chart's `readinessProbe` read, so its green count on a severed store is
//!   the old false-green rate. It is copied rather than remembered so the number is observed.
//! * **after** — `/health/ready`, which observes the store.
//!
//! Beside them, the traffic a false green admits: writes to the same severed instance, counted as
//! 5xx served to clients that the Service should never have routed here.
//!
//! The third assertion is the mirror-image defect, and the one that makes the split a split rather
//! than a fix to one endpoint: `/health/live` stays **green** throughout. A dependency failure must
//! not reach the restarter, because restarting a pod whose store is slow makes the store slower.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use tower::ServiceExt; // oneshot

use lighttrack_core::{
    ApiKey, Benchmark, BenchmarkRun, Dataset, DatasetItem, Job, JobFinish, LimitRule, LlmEvent,
    ModelPriceRow, Project, Rubric, Score,
};
use lighttrack_store::{
    Capabilities, CostRow, Result as StoreResult, Scope, SqliteStore, Store, StoreError, Usage,
};

use crate::redact::Redactor;
use crate::tests_ingest::{ingest, make_key, setup};

/// A real store that can be cut off at a chosen moment, the way a pool dies or a volume detaches
/// *after* a healthy start. Every call fails once severed — which is the whole point: a readiness
/// check that only observed a boot-time flag would still be green here.
struct SeverableStore {
    inner: Arc<SqliteStore>,
    severed: AtomicBool,
    reason: Mutex<String>,
}

impl SeverableStore {
    fn new(inner: Arc<SqliteStore>) -> Self {
        Self {
            inner,
            severed: AtomicBool::new(false),
            reason: Mutex::new(String::new()),
        }
    }

    fn sever(&self, why: &str) {
        *self.reason.lock().unwrap() = why.to_string();
        self.severed.store(true, Ordering::SeqCst);
    }

    fn guard(&self) -> StoreResult<()> {
        if self.severed.load(Ordering::SeqCst) {
            return Err(StoreError::Other(self.reason.lock().unwrap().clone()));
        }
        Ok(())
    }
}

/// Delegate every required method through the severance gate. Written as a macro because the
/// bodies are identical and a hand-copied one that forgot the gate would quietly make the fixture
/// lie about what a severed store does.
macro_rules! severable {
    ($( fn $name:ident (&self $(, $arg:ident : $ty:ty )* ) -> $ret:ty; )*) => {
        $(
            fn $name(&self $(, $arg: $ty)*) -> $ret {
                self.guard()?;
                self.inner.$name($($arg),*)
            }
        )*
    };
}

impl Store for SeverableStore {
    severable! {
        fn init_schema(&self) -> StoreResult<()>;
        fn insert_event(&self, ev: &LlmEvent) -> StoreResult<()>;
        fn list_events(&self, project: Scope<'_>, limit: usize) -> StoreResult<Vec<LlmEvent>>;
        fn cost_summary(&self, project: Scope<'_>) -> StoreResult<Vec<CostRow>>;
        fn usage_since(&self, project: &str, since: DateTime<Utc>) -> StoreResult<Usage>;
        fn create_project(&self, p: &Project) -> StoreResult<()>;
        fn get_project(&self, id: &str) -> StoreResult<Option<Project>>;
        fn list_projects(&self) -> StoreResult<Vec<Project>>;
        fn create_api_key(&self, k: &ApiKey) -> StoreResult<()>;
        fn find_api_key_by_prefix(&self, prefix: &str) -> StoreResult<Option<ApiKey>>;
        fn touch_api_key(&self, id: &str, when: DateTime<Utc>) -> StoreResult<()>;
        fn create_limit_rule(&self, r: &LimitRule) -> StoreResult<()>;
        fn list_limit_rules(&self, project: &str, only_enabled: bool) -> StoreResult<Vec<LimitRule>>;
        fn get_event(&self, scope: Scope<'_>, id: &str) -> StoreResult<Option<LlmEvent>>;
        fn insert_score(&self, s: &Score) -> StoreResult<()>;
        fn list_scores(&self, project: Scope<'_>, limit: usize) -> StoreResult<Vec<Score>>;
        fn scored_event_ids(&self, scope: Scope<'_>, event_ids: &[String]) -> StoreResult<Vec<String>>;
        fn create_benchmark(&self, b: &Benchmark) -> StoreResult<()>;
        fn get_benchmark(&self, scope: Scope<'_>, id: &str) -> StoreResult<Option<Benchmark>>;
        fn list_benchmarks(&self, project: &str) -> StoreResult<Vec<Benchmark>>;
        fn create_benchmark_run(&self, r: &BenchmarkRun) -> StoreResult<()>;
        fn list_benchmark_runs(&self, scope: Scope<'_>, benchmark_id: &str) -> StoreResult<Vec<BenchmarkRun>>;
        fn upsert_price(&self, p: &ModelPriceRow) -> StoreResult<()>;
        fn list_prices(&self) -> StoreResult<Vec<ModelPriceRow>>;
        fn create_dataset(&self, d: &Dataset) -> StoreResult<()>;
        fn get_dataset(&self, scope: Scope<'_>, id: &str) -> StoreResult<Option<Dataset>>;
        fn list_datasets(&self, project: Scope<'_>) -> StoreResult<Vec<Dataset>>;
        fn set_dataset_frozen(&self, scope: Scope<'_>, id: &str, frozen: bool) -> StoreResult<()>;
        fn create_dataset_item(&self, item: &DatasetItem) -> StoreResult<()>;
        fn list_dataset_items(&self, scope: Scope<'_>, dataset_id: &str) -> StoreResult<Vec<DatasetItem>>;
        fn create_rubric(&self, r: &Rubric) -> StoreResult<()>;
        fn get_rubric(&self, scope: Scope<'_>, id: &str) -> StoreResult<Option<Rubric>>;
        fn list_rubrics(&self, project: &str) -> StoreResult<Vec<Rubric>>;
        fn create_job(&self, j: &Job) -> StoreResult<()>;
        fn claim_job(&self, stale_before: DateTime<Utc>, kinds: &[&str]) -> StoreResult<Option<Job>>;
        fn update_job_progress(&self, id: &str, progress: &str) -> StoreResult<()>;
        fn finish_job(&self, id: &str, status: &str, result: &Value, error: Option<&str>, fence: Option<DateTime<Utc>>) -> StoreResult<JobFinish>;
        fn get_job(&self, scope: Scope<'_>, id: &str) -> StoreResult<Option<Job>>;
        fn list_jobs(&self, scope: Scope<'_>, status: Option<&str>, limit: usize) -> StoreResult<Vec<Job>>;
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}

/// The handler `/health` was before the split, reproduced so the before-number is **measured on
/// the old code** rather than argued from memory. A constant cannot observe anything; that is the
/// defect, and this is what it looks like.
async fn legacy_health() -> &'static str {
    "ok"
}

async fn get_status(app: &Router, uri: &str) -> (StatusCode, String) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// How many probes each endpoint answers in a run. Any number ≥ 1 measures the same thing; eight
/// is one readiness period at the chart's cadence, so the counts read as "over one probe window".
const PROBES: usize = 8;

#[tokio::test]
async fn a_severed_store_leaves_the_service_instead_of_collecting_5xx_for_it() {
    let (mut state, sqlite) = setup(Redactor::off());
    let key = make_key(&sqlite, "proj-a");
    let severable = Arc::new(SeverableStore::new(sqlite.clone()));
    state.store = severable.clone();
    let app = crate::build_router(state);

    // A scratch router carrying only the pre-split endpoint, over the same severed state.
    let legacy = Router::new().route("/health", get(legacy_health));

    // --- healthy: every answer is green, and the rollup is still the literal `ok` ---------------
    let (status, body) = get_status(&app, "/health").await;
    assert_eq!(status, StatusCode::OK);
    let parsed: Value = serde_json::from_str(&body).expect("the rollup is JSON");
    assert_eq!(
        parsed["status"], "ok",
        "deploy/README.md, smoke.sh and the Dockerfile HEALTHCHECK read this field"
    );
    assert_eq!(get_status(&app, "/health/live").await.0, StatusCode::OK);
    assert_eq!(get_status(&app, "/health/ready").await.0, StatusCode::OK);

    // --- the dependency dies under a process that is otherwise perfectly fine -------------------
    severable.sever("connection reset by peer");

    let mut legacy_green = 0usize;
    let mut ready_green = 0usize;
    let mut live_green = 0usize;
    for _ in 0..PROBES {
        if get_status(&legacy, "/health").await.0 == StatusCode::OK {
            legacy_green += 1;
        }
        if get_status(&app, "/health/ready").await.0 == StatusCode::OK {
            ready_green += 1;
        }
        if get_status(&app, "/health/live").await.0 == StatusCode::OK {
            live_green += 1;
        }
    }

    // BEFORE: the endpoint the readinessProbe read is green on every single probe while the store
    // is unreachable. The pod stays in the Service.
    assert_eq!(
        legacy_green, PROBES,
        "the pre-split handler is a constant — this is the false-green rate the split removes"
    );
    // AFTER: readiness observes the store, so the pod leaves the Service.
    assert_eq!(
        ready_green, 0,
        "readiness must go red when the store cannot answer"
    );
    // AND: liveness stays green, so the kubelet does not restart a pod whose only problem is a
    // dependency. Fixing readiness without this converts a false green into a crash loop.
    assert_eq!(
        live_green, PROBES,
        "a dependency failure must never reach the restarter"
    );

    // The traffic a false green admits: every write served here is a 5xx the client should never
    // have been routed to.
    let mut served_5xx = 0usize;
    for _ in 0..PROBES {
        let (status, _) = ingest(
            &app,
            &key,
            json!({
                "provider": "anthropic",
                "model": "claude-haiku-4-5",
                "usage": { "input": 10, "output": 5 }
            }),
        )
        .await;
        if status.is_server_error() {
            served_5xx += 1;
        }
    }
    assert_eq!(
        served_5xx, PROBES,
        "while the old probe was green, every routed write failed"
    );

    // The rollup names the member rather than laundering it into a bare status code.
    let (status, body) = get_status(&app, "/health").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        body.contains("store: down"),
        "the rollup must name its red member: {body}"
    );
    assert!(
        body.contains("connection reset"),
        "and carry the reason: {body}"
    );
}

#[tokio::test]
async fn liveness_observes_nothing_outside_the_process() {
    // `live` takes no state at all — the type system is the enforcement — but the wired route is
    // what the chart points at, so pin it there too.
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);
    let (status, body) = get_status(&app, "/health/live").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "live");
}
