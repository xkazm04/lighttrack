//! End-to-end tests for the Collective hub ingest/leaderboard path over the **wired axum router**.
//!
//! These pin the hardening guarantees that no pure unit test covers: ingest is gated by the accept
//! flag; an unknown digest schema is rejected; the contributor identity is derived from a **hub-issued
//! credential** (the body's id is ignored, and an arbitrary bearer string is not an identity at all)
//! so two keys land under two ids and one key can only replace its own set; under-k buckets are
//! dropped and counted; and a keyless push is refused unless the hub opts into anonymous
//! contributions.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use lighttrack_core::{ModelAliases, PriceBook};
use lighttrack_store::{SqliteStore, Store};

use crate::auth::AuthMode;
use crate::collective::Collective;
use crate::state::AppState;

/// App state over a fresh in-memory store, in **dev** auth mode (so a keyless or arbitrary-bearer
/// request reaches the collective handler), with a hub configured by the given knobs. Uses a small
/// fixed alias table so normalization is deterministic regardless of the on-disk config file.
fn setup(accept: bool, allow_anon: bool, min_cases: u32) -> (AppState, Arc<SqliteStore>) {
    // min_contributors=1 preserves the single-contributor behavior most of these tests exercise;
    // the k-anonymity tests use `setup_k` with a real floor.
    setup_k(accept, allow_anon, min_cases, 1)
}

fn setup_k(
    accept: bool,
    allow_anon: bool,
    min_cases: u32,
    min_contributors: u32,
) -> (AppState, Arc<SqliteStore>) {
    let aliases = ModelAliases::from_json_str(
        r#"{"providers":{"azure-openai":"openai"},"models":{"gpt-4o-2024-08-06":"gpt-4o"}}"#,
    )
    .unwrap();
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let dyn_store: Arc<dyn Store + Send + Sync> = store.clone();
    let state = AppState {
        store: dyn_store,
        prices: Arc::new(RwLock::new(PriceBook::new(HashMap::new()))),
        auth_mode: AuthMode::Dev,
        admin_key: None,
        relay_device_key: None,
        relay_flat_cost: 1.0,
        alerts: Arc::new(crate::alerts::Alerter::from_env()),
        redact: Arc::new(crate::redact::Redactor::off()),
        billing: Arc::new(lighttrack_billing::BillingRegistry::from_env()),
        collective: Arc::new(Collective {
            contributor_id: "anonymous".to_string(),
            accept,
            allow_anon,
            min_cases,
            display_floor: 30,
            min_contributors,
            min_interval_hours: 0,
            max_age_days: 90,
            aliases,
            alias_source: "test fixture".to_string(),
        }),
        seen_webhooks: Arc::new(crate::idempotency::SeenWebhooks::new(
            crate::idempotency::DEFAULT_CAPACITY,
        )),
        rejections: Arc::new(crate::rejections::RejectionLedger::new()),
        ingest_guard: Arc::new(crate::shed::IngestGuard::from_env()),
        auth_throttle: Arc::new(crate::auth_throttle::AuthThrottle::from_env()),
        redaction_policies: Arc::new(crate::state::RedactionCache::new(HashMap::new())),
    };
    (state, store)
}

/// Mint a real hub-issued **contributor** credential: a project carrying `collective_opt_in` plus an
/// API key on it. Returns the full key string. This is the only kind of token a hub accepts a
/// contribution from — an arbitrary bearer string is not an identity (see `resolve_contributor`).
fn contributor_key(store: &SqliteStore, name: &str) -> String {
    mint_key(store, name, true)
}

fn mint_key(store: &SqliteStore, name: &str, opt_in: bool) -> String {
    use chrono::Utc;
    use lighttrack_core::{new_id, ApiKey, Project, Redaction};
    let project_id = format!("proj-{name}");
    store
        .create_project(&Project {
            id: project_id.clone(),
            name: project_id.clone(),
            enabled: true,
            redaction: Redaction::None,
            collective_opt_in: opt_in,
            created_at: Utc::now(),
        })
        .unwrap();
    let g = crate::auth::generate_key();
    store
        .create_api_key(&ApiKey {
            id: new_id(),
            project_id,
            name: name.into(),
            prefix: g.prefix.clone(),
            key_hash: g.key_hash.clone(),
            created_at: Utc::now(),
            last_used_at: None,
            revoked: false,
        })
        .unwrap();
    g.full_key
}

fn entry(model: &str, q: f64, cases: u32) -> Value {
    json!({
        "provider": "anthropic", "model": model, "task_type": "qa",
        "quality": q, "pass_rate": q, "avg_cost_usd": 0.003,
        "p50_latency_ms": 900, "p95_latency_ms": 2000, "n_runs": 1, "n_cases": cases
    })
}

/// POST a digest to /v1/collective/ingest with an optional bearer token.
async fn ingest(app: &Router, token: Option<&str>, digest: Value) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/collective/ingest")
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(digest.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, v)
}

async fn leaderboard(app: &Router) -> (StatusCode, Value) {
    leaderboard_q(app, "").await
}

async fn leaderboard_q(app: &Router, query: &str) -> (StatusCode, Value) {
    let uri = if query.is_empty() {
        "/v1/collective/leaderboard".to_string()
    } else {
        format!("/v1/collective/leaderboard?{query}")
    };
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// A v2 digest carrying one bucket with an explicit provider/model and judge provider.
fn digest_of(provider: &str, model: &str, q: f64, cases: u32, judge: &str) -> Value {
    json!({ "schema_version": 2, "entries": [{
        "provider": provider, "model": model, "task_type": "qa",
        "quality": q, "pass_rate": q, "avg_cost_usd": 0.003,
        "p50_latency_ms": 900, "p95_latency_ms": 2000, "n_runs": 1, "n_cases": cases,
        "judge_provider": judge
    }]})
}

#[tokio::test]
async fn ingest_refused_unless_accept_flag_set() {
    let (state, _) = setup(false, false, 5);
    let app = crate::build_router(state);
    let (status, body) = ingest(
        &app,
        Some("some-key"),
        json!({ "entries": [entry("haiku", 0.8, 10)] }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "forbidden", "{body}");
}

#[tokio::test]
async fn unknown_schema_version_is_rejected_400() {
    let (state, _) = setup(true, false, 5);
    let app = crate::build_router(state);
    let (status, body) = ingest(
        &app,
        Some("some-key"),
        json!({ "schema_version": 999, "entries": [entry("haiku", 0.8, 10)] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn identity_is_derived_from_the_key_not_the_body() {
    // A poster claims contributor_id "c-victim" in the body but presents its own key: the stored id
    // must be derived from the key, and must NOT be the claimed one.
    let (state, store) = setup(true, false, 5);
    let key = contributor_key(&store, "attacker");
    let app = crate::build_router(state);
    let (status, ack) = ingest(
        &app,
        Some(&key),
        json!({ "contributor_id": "c-victim", "entries": [entry("haiku", 0.8, 10)] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["accepted"], 1);
    let stored = store.list_collective_entries().unwrap();
    assert_eq!(stored.len(), 1);
    assert_ne!(
        stored[0].contributor_id, "c-victim",
        "body-supplied id must be ignored"
    );
    assert!(
        stored[0].contributor_id.starts_with("c-"),
        "id is key-derived"
    );
    assert_eq!(ack["contributor_id"], stored[0].contributor_id);
}

#[tokio::test]
async fn different_keys_land_under_different_ids_no_overwrite() {
    // Two keys both claim the same body id and the same model bucket. Because the identity is derived
    // from the key, they must NOT collide: two rows survive, not one overwriting the other.
    let (state, store) = setup(true, false, 5);
    let (alpha, beta) = (
        contributor_key(&store, "alpha"),
        contributor_key(&store, "beta"),
    );
    let app = crate::build_router(state);
    let body = |q: f64| json!({ "contributor_id": "c-shared", "entries": [entry("haiku", q, 10)] });

    let (s1, _) = ingest(&app, Some(&alpha), body(0.7)).await;
    let (s2, _) = ingest(&app, Some(&beta), body(0.9)).await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));

    let stored = store.list_collective_entries().unwrap();
    assert_eq!(
        stored.len(),
        2,
        "distinct keys must not overwrite each other"
    );
    let ids: std::collections::BTreeSet<_> =
        stored.iter().map(|e| e.contributor_id.clone()).collect();
    assert_eq!(ids.len(), 2, "two distinct derived ids");

    // The leaderboard therefore reports two contributors for the shared bucket.
    let (ls, lb) = leaderboard(&app).await;
    assert_eq!(ls, StatusCode::OK);
    assert_eq!(lb["rows"][0]["n_contributors"], 2, "{lb}");
}

#[tokio::test]
async fn same_key_replaces_its_own_set() {
    // Re-contributing under the same key replaces, never accretes.
    let (state, store) = setup(true, false, 5);
    let alpha = contributor_key(&store, "alpha");
    let app = crate::build_router(state);
    let (_s, _) = ingest(
        &app,
        Some(&alpha),
        json!({ "entries": [entry("haiku", 0.7, 10), entry("sonnet", 0.8, 10)] }),
    )
    .await;
    assert_eq!(store.list_collective_entries().unwrap().len(), 2);
    // Second push from the same key with a single bucket → the dropped one must not linger.
    let (_s, _) = ingest(
        &app,
        Some(&alpha),
        json!({ "entries": [entry("haiku", 0.9, 20)] }),
    )
    .await;
    let stored = store.list_collective_entries().unwrap();
    assert_eq!(stored.len(), 1, "re-contribution replaces the whole set");
    assert_eq!(stored[0].model, "haiku");
    assert_eq!(stored[0].n_cases, 20);
}

#[tokio::test]
async fn under_k_buckets_are_dropped_and_counted() {
    let (state, store) = setup(true, false, 5);
    let key = contributor_key(&store, "a");
    let app = crate::build_router(state);
    // One bucket clears the floor (10 ≥ 5), one is below it (3 < 5).
    let (status, ack) = ingest(
        &app,
        Some(&key),
        json!({ "min_cases": 1, "entries": [entry("haiku", 0.8, 10), entry("sonnet", 0.9, 3)] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["accepted"], 1, "{ack}");
    assert_eq!(ack["dropped_under_min"], 1, "{ack}");
    let stored = store.list_collective_entries().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].model, "haiku");
}

#[tokio::test]
async fn v1_digest_accepted_and_lands_with_null_variance() {
    // A legacy v1 digest (schema_version 1, no quality_variance) must not be orphaned by the v2 bump.
    let (state, store) = setup(true, false, 5);
    let key = contributor_key(&store, "a");
    let app = crate::build_router(state);
    let (status, ack) = ingest(
        &app,
        Some(&key),
        json!({ "schema_version": 1, "entries": [entry("haiku", 0.8, 10)] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["accepted"], 1);
    let stored = store.list_collective_entries().unwrap();
    assert!(
        stored[0].quality_variance.is_none(),
        "v1 entry lands with variance NULL"
    );
}

#[tokio::test]
async fn v2_variance_is_carried_through_to_storage_and_ci() {
    // Two contributors report variance over enough cases → the leaderboard row carries a CI.
    let (state, store) = setup(true, false, 5);
    let (ka, kb) = (contributor_key(&store, "a"), contributor_key(&store, "b"));
    let app = crate::build_router(state);
    let with_var = |model: &str, q: f64, var: f64, cases: u32| {
        json!({ "schema_version": 2, "entries": [{
            "provider": "anthropic", "model": model, "task_type": "qa",
            "quality": q, "pass_rate": q, "avg_cost_usd": 0.003,
            "p50_latency_ms": 900, "p95_latency_ms": 2000,
            "n_runs": 3, "n_cases": cases, "quality_variance": var
        }]})
    };
    ingest(&app, Some(&ka), with_var("haiku", 0.80, 0.04, 100)).await;
    ingest(&app, Some(&kb), with_var("haiku", 0.84, 0.04, 100)).await;

    let stored = store.list_collective_entries().unwrap();
    assert!(
        stored.iter().all(|e| e.quality_variance == Some(0.04)),
        "variance persisted"
    );

    let (ls, lb) = leaderboard(&app).await;
    assert_eq!(ls, StatusCode::OK);
    let row = &lb["rows"][0];
    assert!(
        row["quality_ci95"].as_f64().is_some(),
        "full variance coverage → CI present: {lb}"
    );
    assert_eq!(row["p95_latency_ms"], 2000, "worst-observed p95 surfaced");
    assert_eq!(row["low_confidence"], false, "200 cases clears the floor");
}

#[tokio::test]
async fn digest_includes_only_consenting_projects() {
    use chrono::Utc;
    use lighttrack_core::{new_id, Benchmark, BenchmarkCase, BenchmarkRun, Project, Redaction};

    let (state, store) = setup(true, false, 1);
    let app = crate::build_router(state);

    // Two projects with identical benchmark runs; only one has consented.
    let mk_project = |id: &str, opt_in: bool| Project {
        id: id.into(),
        name: id.into(),
        enabled: true,
        redaction: Redaction::None,
        collective_opt_in: opt_in,
        created_at: Utc::now(),
    };
    let mk_bench_run = |project: &str, model: &str| {
        let b = Benchmark {
            id: new_id(),
            project_id: project.into(),
            name: "qa bench".into(),
            rubric: "is it right".into(),
            judge_model: "haiku".into(),
            target: json!({ "provider": "anthropic", "model": model }),
            dataset_ref: None,
            rubric_id: None,
            dataset: vec![BenchmarkCase {
                input: "2+2".into(),
                expected: None,
                output: None,
            }],
            baseline_score: None,
            created_at: Utc::now(),
        };
        store.create_benchmark(&b).unwrap();
        store
            .create_benchmark_run(&BenchmarkRun {
                id: new_id(),
                benchmark_id: b.id.clone(),
                started_at: Utc::now(),
                finished_at: Some(Utc::now()),
                n_cases: 10,
                mean_score: Some(0.9),
                pass_rate: Some(0.9),
                cost_usd: 0.1,
                status: "passed".into(),
                p50_latency_ms: Some(100),
                p95_latency_ms: Some(200),
                total_tokens: Some(100),
                report: Value::Null,
            })
            .unwrap();
    };
    store
        .create_project(&mk_project("proj-nda", false))
        .unwrap();
    store
        .create_project(&mk_project("proj-open", true))
        .unwrap();
    mk_bench_run("proj-nda", "secret-model");
    mk_bench_run("proj-open", "public-model");

    let req = Request::builder()
        .method("GET")
        .uri("/v1/collective/digest")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let digest: Value = serde_json::from_slice(&bytes).unwrap();

    let blob = digest.to_string();
    assert!(
        blob.contains("public-model"),
        "consenting project's runs are in the digest: {blob}"
    );
    assert!(
        !blob.contains("secret-model"),
        "a project that never opted in must not ship in the digest: {blob}"
    );
    assert_eq!(
        digest["projects_included"], 1,
        "consent envelope disclosed: {digest}"
    );
    assert_eq!(digest["projects_excluded"], 1);
}

#[tokio::test]
async fn single_source_rows_are_withheld_below_the_contributor_floor() {
    // Hub with a real source floor (k=2): a row backed by one contributor must never surface —
    // however many cases it has, and no filter may resurrect it.
    let (state, store) = setup_k(true, false, 5, 2);
    let (solo, ka, kb) = (
        contributor_key(&store, "solo"),
        contributor_key(&store, "a"),
        contributor_key(&store, "b"),
    );
    let app = crate::build_router(state);
    // One lone contributor benchmarks cohere; two contributors overlap on haiku.
    ingest(
        &app,
        Some(&solo),
        digest_of("cohere", "command-r", 0.9, 5000, "openai"),
    )
    .await;
    ingest(
        &app,
        Some(&ka),
        digest_of("anthropic", "haiku", 0.80, 100, "anthropic"),
    )
    .await;
    ingest(
        &app,
        Some(&kb),
        digest_of("anthropic", "haiku", 0.84, 100, "anthropic"),
    )
    .await;

    let (ls, lb) = leaderboard(&app).await;
    assert_eq!(ls, StatusCode::OK);
    let rows = lb["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "only the 2-source row is visible: {lb}");
    assert_eq!(rows[0]["model"], "haiku");
    assert_eq!(
        lb["held_back"], 1,
        "the withheld row is disclosed, not silently dropped"
    );

    // A provider filter must not strip the board down to the lone source.
    let (fs, fb) = leaderboard_q(&app, "provider=cohere").await;
    assert_eq!(fs, StatusCode::OK);
    assert!(
        fb["rows"].as_array().unwrap().is_empty(),
        "filter cannot resurrect a 1-source row: {fb}"
    );

    // The same data on a single-tenant hub (k=1, the explicit opt-out) shows everything.
    let (state1, store1) = setup_k(true, false, 5, 1);
    let solo1 = contributor_key(&store1, "solo");
    let app1 = crate::build_router(state1);
    ingest(
        &app1,
        Some(&solo1),
        digest_of("cohere", "command-r", 0.9, 5000, "openai"),
    )
    .await;
    let (s1, b1) = leaderboard(&app1).await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(
        b1["rows"].as_array().unwrap().len(),
        1,
        "k=1 opts out of the source floor"
    );
    assert_eq!(b1["held_back"], 0);
}

#[tokio::test]
async fn ingest_normalizes_identity_so_variants_merge_into_one_row() {
    // Two contributors report the same model spelled differently; normalization collapses them.
    let (state, store) = setup(true, false, 5);
    let (ka, kb) = (contributor_key(&store, "a"), contributor_key(&store, "b"));
    let app = crate::build_router(state);
    ingest(
        &app,
        Some(&ka),
        digest_of("openai", "gpt-4o-2024-08-06", 0.80, 100, "openai"),
    )
    .await;
    ingest(
        &app,
        Some(&kb),
        digest_of("azure-openai", "gpt-4o", 0.84, 100, "openai"),
    )
    .await;

    let (ls, lb) = leaderboard(&app).await;
    assert_eq!(ls, StatusCode::OK);
    assert_eq!(
        lb["rows"].as_array().unwrap().len(),
        1,
        "variants merged into one row: {lb}"
    );
    let row = &lb["rows"][0];
    assert_eq!(row["provider"], "openai");
    assert_eq!(row["model"], "gpt-4o");
    assert_eq!(row["n_contributors"], 2);
}

#[tokio::test]
async fn mixed_judges_annotated_and_judge_filter_works() {
    let (state, store) = setup(true, false, 5);
    let (ka, kb) = (contributor_key(&store, "a"), contributor_key(&store, "b"));
    let app = crate::build_router(state);
    // Same bucket judged by two different providers across contributors.
    ingest(
        &app,
        Some(&ka),
        digest_of("anthropic", "haiku", 0.80, 100, "anthropic"),
    )
    .await;
    ingest(
        &app,
        Some(&kb),
        digest_of("anthropic", "haiku", 0.84, 100, "openai"),
    )
    .await;

    let (_s, lb) = leaderboard(&app).await;
    let row = &lb["rows"][0];
    assert_eq!(row["mixed_judges"], 2, "distinct judges annotated: {lb}");
    let judges = row["judge_providers"].as_array().unwrap();
    assert_eq!(judges.len(), 2);

    // ?judge=anthropic keeps the row (it was partly judged by anthropic); ?judge=cohere drops it.
    let (_s, kept) = leaderboard_q(&app, "judge=anthropic").await;
    assert_eq!(kept["rows"].as_array().unwrap().len(), 1, "{kept}");
    let (_s, dropped) = leaderboard_q(&app, "judge=cohere").await;
    assert_eq!(dropped["rows"].as_array().unwrap().len(), 0, "{dropped}");
}

#[tokio::test]
async fn header_counts_are_computed_over_the_filtered_rows() {
    // Two contributors, each the *sole* backer of a distinct provider's row. A provider filter that
    // hides one contributor's only row must drop it from the header `contributors` count too — header
    // and rows can no longer disagree.
    let (state, store) = setup(true, false, 5);
    let (ka, kb) = (contributor_key(&store, "a"), contributor_key(&store, "b"));
    let app = crate::build_router(state);
    ingest(
        &app,
        Some(&ka),
        digest_of("anthropic", "haiku", 0.80, 100, "anthropic"),
    )
    .await;
    ingest(
        &app,
        Some(&kb),
        digest_of("openai", "gpt-x", 0.84, 100, "openai"),
    )
    .await;

    // Unfiltered: both contributors and both models are visible.
    let (ls, lb) = leaderboard(&app).await;
    assert_eq!(ls, StatusCode::OK);
    assert_eq!(lb["contributors"], 2, "{lb}");
    assert_eq!(lb["n_models"], 2, "{lb}");
    assert_eq!(lb["n_rows"], 2, "{lb}");

    // Filter to anthropic: only key-a's row survives, so the header must report one contributor.
    let (_s, only_a) = leaderboard_q(&app, "provider=anthropic").await;
    assert_eq!(only_a["rows"].as_array().unwrap().len(), 1, "{only_a}");
    assert_eq!(
        only_a["contributors"], 1,
        "excluded contributor drops from the count: {only_a}"
    );
    assert_eq!(only_a["n_models"], 1, "{only_a}");
    assert_eq!(only_a["n_rows"], 1, "{only_a}");
}

#[tokio::test]
async fn n_models_is_distinct_models_not_row_count() {
    // One contributor, one model, under two task types → two rows but a single distinct model.
    let (state, store) = setup(true, false, 5);
    let ka = contributor_key(&store, "a");
    let app = crate::build_router(state);
    let two_tasks = json!({ "schema_version": 2, "entries": [
        {"provider":"anthropic","model":"haiku","task_type":"qa",
         "quality":0.8,"pass_rate":0.8,"avg_cost_usd":0.003,"n_runs":1,"n_cases":100},
        {"provider":"anthropic","model":"haiku","task_type":"summarization",
         "quality":0.7,"pass_rate":0.7,"avg_cost_usd":0.003,"n_runs":1,"n_cases":100}
    ]});
    ingest(&app, Some(&ka), two_tasks).await;

    let (ls, lb) = leaderboard(&app).await;
    assert_eq!(ls, StatusCode::OK);
    assert_eq!(
        lb["rows"].as_array().unwrap().len(),
        2,
        "two task-type rows: {lb}"
    );
    assert_eq!(lb["n_rows"], 2, "{lb}");
    assert_eq!(lb["n_models"], 1, "one distinct (provider, model): {lb}");
    assert_eq!(lb["contributors"], 1, "{lb}");
}

#[tokio::test]
async fn varying_bearer_strings_cannot_manufacture_sources() {
    // THE dev-mode forgery: `authenticate` is lenient in dev (any unrecognized token → Principal::Dev),
    // so hashing the presented token would let one poster mint N ids and walk through min_contributors.
    // Deriving from a hub-issued credential closes it: every made-up token is refused outright…
    let (state, store) = setup_k(true, false, 5, 2);
    let app = crate::build_router(state);
    for tok in ["forged-1", "forged-2", "forged-3", "forged-4"] {
        let (status, body) = ingest(
            &app,
            Some(tok),
            digest_of("anthropic", "haiku", 0.99, 5000, "openai"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "made-up token {tok} must not be an identity: {body}"
        );
    }
    assert!(
        store.list_collective_entries().unwrap().is_empty(),
        "nothing was forged into the hub"
    );

    // …and when the hub opts into anonymous contributions, they all collapse into ONE identity, so the
    // source floor still cannot be defeated — the row stays withheld.
    let (state, store) = setup_k(true, true, 5, 2);
    let app = crate::build_router(state);
    for tok in ["forged-1", "forged-2", "forged-3", "forged-4"] {
        let (status, _) = ingest(
            &app,
            Some(tok),
            digest_of("anthropic", "haiku", 0.99, 5000, "openai"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let ids: std::collections::BTreeSet<_> = store
        .list_collective_entries()
        .unwrap()
        .into_iter()
        .map(|e| e.contributor_id)
        .collect();
    assert_eq!(
        ids.len(),
        1,
        "every uncredentialed poster is one source, not four: {ids:?}"
    );
    let (_s, lb) = leaderboard(&app).await;
    assert!(
        lb["rows"].as_array().unwrap().is_empty(),
        "k=2 still holds the row back: {lb}"
    );
    assert_eq!(lb["held_back"], 1);
}

#[tokio::test]
async fn a_non_consenting_projects_key_is_not_a_contributor_credential() {
    // An ordinary ingest key belongs to a project that never opted into the collective: it may push
    // events all day, but it cannot contribute to the leaderboard.
    let (state, store) = setup(true, false, 5);
    let ingest_only = mint_key(&store, "plain", false);
    let app = crate::build_router(state);
    let (status, body) = ingest(
        &app,
        Some(&ingest_only),
        json!({ "entries": [entry("haiku", 0.8, 10)] }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(store.list_collective_entries().unwrap().is_empty());

    // The same project, once it consents, contributes fine.
    let mut p = store.get_project("proj-plain").unwrap().unwrap();
    p.collective_opt_in = true;
    store.update_project(&p).unwrap();
    let (status, ack) = ingest(
        &app,
        Some(&ingest_only),
        json!({ "entries": [entry("haiku", 0.8, 10)] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["accepted"], 1);
}

#[tokio::test]
async fn an_absurd_case_claim_cannot_own_a_row() {
    // Two honest contributors agree a model is good. A third claims a billion cases saying it is
    // terrible. Flat case-weighted pooling hands the row to whoever types the biggest number.
    let (state, store) = setup_k(true, false, 5, 2);
    let (ka, kb, liar) = (
        contributor_key(&store, "a"),
        contributor_key(&store, "b"),
        contributor_key(&store, "liar"),
    );
    let app = crate::build_router(state);
    ingest(
        &app,
        Some(&ka),
        digest_of("anthropic", "haiku", 0.80, 100, "anthropic"),
    )
    .await;
    ingest(
        &app,
        Some(&kb),
        digest_of("anthropic", "haiku", 0.82, 100, "anthropic"),
    )
    .await;

    // A billion cases from one instance is not a benchmark result, it is a typo or an attack.
    let (status, ack) = ingest(
        &app,
        Some(&liar),
        digest_of("anthropic", "haiku", 0.05, 1_000_000_000, "anthropic"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(
        ack["accepted"], 0,
        "an implausible claim is not stored: {ack}"
    );
    assert_eq!(
        ack["rejected_implausible"], 1,
        "…and it is disclosed, not silently dropped: {ack}"
    );

    let (_s, lb) = leaderboard(&app).await;
    let row = &lb["rows"][0];
    let q = row["quality"].as_f64().unwrap();
    assert!(
        q > 0.75,
        "the honest consensus survives; got quality {q} in {lb}"
    );

    // Even a claim *within* the plausible ceiling cannot own the row: a single source's weight is
    // winsorized to the documented share ceiling, and the row discloses that share.
    let (status, ack) = ingest(
        &app,
        Some(&liar),
        digest_of("anthropic", "haiku", 0.05, 999_999, "anthropic"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(
        ack["accepted"], 1,
        "a big-but-plausible contribution is still accepted: {ack}"
    );

    let (_s, lb) = leaderboard(&app).await;
    let row = &lb["rows"][0];
    let share = row["max_source_share"]
        .as_f64()
        .expect("provenance is visible");
    assert!(
        share <= 0.801,
        "no source exceeds the documented share ceiling; got {share} in {lb}"
    );
    let q = row["quality"].as_f64().unwrap();
    assert!(
        q > 0.20,
        "one source may lead a row but not own it outright; got {q} in {lb}"
    );
}

#[tokio::test]
async fn a_genuinely_large_single_source_still_outweighs_a_small_one() {
    // The non-goal: do not over-correct into ignoring sample size. 10k cases must still beat 10.
    let (state, store) = setup_k(true, false, 5, 1);
    let (big, small) = (
        contributor_key(&store, "big"),
        contributor_key(&store, "small"),
    );
    let app = crate::build_router(state);
    ingest(
        &app,
        Some(&big),
        digest_of("anthropic", "haiku", 0.90, 10_000, "anthropic"),
    )
    .await;
    ingest(
        &app,
        Some(&small),
        digest_of("anthropic", "haiku", 0.10, 10, "anthropic"),
    )
    .await;

    let (_s, lb) = leaderboard(&app).await;
    let q = lb["rows"][0]["quality"].as_f64().unwrap();
    assert!(
        q > 0.7,
        "the 10k-case run dominates the 10-case one, as it should; got {q} in {lb}"
    );
}

/// DELETE /v1/collective/contribution with an optional bearer token and `?contributor=`.
async fn withdraw(app: &Router, token: Option<&str>, who: Option<&str>) -> (StatusCode, Value) {
    let uri = match who {
        Some(c) => format!("/v1/collective/contribution?contributor={c}"),
        None => "/v1/collective/contribution".to_string(),
    };
    let mut req = Request::builder().method("DELETE").uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, v)
}

#[tokio::test]
async fn a_contributor_can_withdraw_its_own_contribution_and_only_its_own() {
    let (state, store) = setup(true, false, 5);
    let (ka, kb) = (contributor_key(&store, "a"), contributor_key(&store, "b"));
    let app = crate::build_router(state);
    ingest(
        &app,
        Some(&ka),
        digest_of("anthropic", "haiku", 0.80, 100, "anthropic"),
    )
    .await;
    ingest(
        &app,
        Some(&kb),
        digest_of("anthropic", "haiku", 0.84, 100, "anthropic"),
    )
    .await;
    assert_eq!(store.list_collective_entries().unwrap().len(), 2);

    // A withdraws: its rows go, B's stay. Consent is revocable, not one-way.
    let (status, ack) = withdraw(&app, Some(&ka), None).await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["deleted"], 1, "{ack}");
    let left = store.list_collective_entries().unwrap();
    assert_eq!(left.len(), 1, "only the withdrawing source is removed");

    // A cannot withdraw B's contribution.
    let victim = left[0].contributor_id.clone();
    let (status, body) = withdraw(&app, Some(&ka), Some(&victim)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(store.list_collective_entries().unwrap().len(), 1);

    // Withdrawing twice is harmless (nothing left to delete).
    let (status, ack) = withdraw(&app, Some(&ka), None).await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["deleted"], 0);
}

#[tokio::test]
async fn expired_entries_stop_being_published_and_are_swept() {
    use chrono::{Duration, Utc};
    let (state, store) = setup(true, false, 5);
    let ka = contributor_key(&store, "a");
    let app = crate::build_router(state);
    ingest(
        &app,
        Some(&ka),
        digest_of("anthropic", "haiku", 0.80, 100, "anthropic"),
    )
    .await;

    // Age the stored row past the 90-day retention policy.
    let mut aged = store.list_collective_entries().unwrap().remove(0);
    aged.received_at = Utc::now() - Duration::days(120);
    store.upsert_collective_entry(&aged).unwrap();

    let (_s, lb) = leaderboard(&app).await;
    assert!(
        lb["rows"].as_array().unwrap().is_empty(),
        "an expired row is not published: {lb}"
    );
    assert_eq!(lb["contributors"], 0);

    // …and the next write sweeps it off disk rather than letting it accumulate forever.
    let kb = contributor_key(&store, "b");
    ingest(
        &app,
        Some(&kb),
        digest_of("openai", "gpt-x", 0.7, 100, "openai"),
    )
    .await;
    let left = store.list_collective_entries().unwrap();
    assert_eq!(left.len(), 1, "the expired row was purged: {left:?}");
    assert_eq!(left[0].provider, "openai");
}

#[tokio::test]
async fn a_hub_can_rate_limit_re_pushes_to_blunt_differencing() {
    let (mut state, store) = setup(true, false, 5);
    // One contribution per source per day: a hub operator gets a daily changelog at best, not a live
    // feed of what changed inside a contributor's private suite.
    let c = Arc::get_mut(&mut state.collective).unwrap();
    c.min_interval_hours = 24;
    let ka = contributor_key(&store, "a");
    let app = crate::build_router(state);

    let (s1, _) = ingest(
        &app,
        Some(&ka),
        digest_of("anthropic", "haiku", 0.80, 100, "anthropic"),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let (s2, body) = ingest(
        &app,
        Some(&ka),
        digest_of("anthropic", "haiku", 0.81, 101, "anthropic"),
    )
    .await;
    assert_eq!(s2, StatusCode::TOO_MANY_REQUESTS, "{body}");
    // The first contribution is untouched — a refused re-push must not clear the set.
    let stored = store.list_collective_entries().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].n_cases, 100);
}

#[tokio::test]
async fn published_cost_is_bucketed_so_it_cannot_fingerprint_a_contributor() {
    let (state, store) = setup(true, false, 5);
    let ka = contributor_key(&store, "a");
    let app = crate::build_router(state);
    let with_cost = |cost: f64| {
        json!({ "schema_version": 2, "entries": [{
            "provider": "anthropic", "model": "haiku", "task_type": "qa",
            "quality": 0.8, "pass_rate": 0.8, "avg_cost_usd": cost,
            "n_runs": 1, "n_cases": 100
        }]})
    };
    ingest(&app, Some(&ka), with_cost(0.003_141_59)).await;
    let stored = store.list_collective_entries().unwrap();
    assert_eq!(
        stored[0].avg_cost_usd, 0.0031,
        "a distinctive cost is stored coarsened, not verbatim"
    );
    let (_s, lb) = leaderboard(&app).await;
    assert_eq!(lb["rows"][0]["avg_cost_usd"], 0.0031, "{lb}");
}

#[tokio::test]
async fn anonymous_push_refused_unless_allowed() {
    // Keyless push, hub does not allow anon → 403.
    let (state, _) = setup(true, false, 5);
    let app = crate::build_router(state);
    let (status, body) = ingest(&app, None, json!({ "entries": [entry("haiku", 0.8, 10)] })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Same push, hub opts into anon → accepted under the shared identity.
    let (state, store) = setup(true, true, 5);
    let app = crate::build_router(state);
    let (status, ack) = ingest(&app, None, json!({ "entries": [entry("haiku", 0.8, 10)] })).await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["contributor_id"], "anonymous");
    assert_eq!(
        store.list_collective_entries().unwrap()[0].contributor_id,
        "anonymous"
    );
}

/// A v3 digest carrying one bucket with an explicit rigor block.
fn rigor_digest(model: &str, determinism: &str, frozen: &str, tested: &str) -> Value {
    json!({ "schema_version": 3, "entries": [{
        "provider": "anthropic", "model": model, "task_type": "qa",
        "quality": 0.8, "pass_rate": 0.8, "avg_cost_usd": 0.003,
        "n_runs": 2, "n_cases": 100,
        "determinism": determinism, "frozen_dataset": frozen, "significance_tested": tested
    }]})
}

#[tokio::test]
async fn rigor_rides_the_row_and_the_leaderboard_can_be_filtered_by_it() {
    let (state, store) = setup_k(true, false, 5, 2);
    let (ka, kb) = (contributor_key(&store, "a"), contributor_key(&store, "b"));
    let app = crate::build_router(state);
    // `strict` is a pinned, frozen, significance-tested contribution; `loose` sampled a mutable set.
    ingest(
        &app,
        Some(&ka),
        rigor_digest("haiku", "exact", "all", "all"),
    )
    .await;
    let (_s, lb) = leaderboard(&app).await;
    assert_eq!(
        lb["n_rows"], 0,
        "one source is still one source — the k floor comes first: {lb}"
    );

    ingest(
        &app,
        Some(&kb),
        rigor_digest("haiku", "exact", "all", "all"),
    )
    .await;
    let (_s, lb) = leaderboard(&app).await;
    let row = &lb["rows"][0];
    assert_eq!(row["rigor"]["determinism"], "exact", "{lb}");
    assert_eq!(row["rigor"]["frozen_dataset"], "all");
    assert_eq!(row["rigor"]["significance_tested"], "all");
    assert_eq!(row["mixed_rigor"], false);
    // Asking for the rigorous slice keeps it; asking for the sloppy slice does not.
    let (_s, lb) = leaderboard_q(&app, "determinism=exact&frozen_dataset=true").await;
    assert_eq!(lb["n_rows"], 1, "{lb}");
    let (_s, lb) = leaderboard_q(&app, "determinism=sampled").await;
    assert_eq!(lb["n_rows"], 0, "{lb}");

    // Now B re-contributes the same bucket from a sloppy run: the row discloses the mixture instead
    // of averaging it away, and drops out of the "exact, frozen" slice.
    ingest(
        &app,
        Some(&kb),
        rigor_digest("haiku", "sampled", "none", "all"),
    )
    .await;
    let (_s, lb) = leaderboard(&app).await;
    let row = &lb["rows"][0];
    assert_eq!(
        row["rigor"]["determinism"], "sampled",
        "the headline is the weakest stamp: {lb}"
    );
    assert_eq!(
        row["rigor"]["determinism_levels"],
        json!(["exact", "sampled"])
    );
    assert_eq!(row["rigor"]["frozen_dataset"], "mixed");
    assert_eq!(
        row["rigor"]["significance_tested"], "all",
        "both sources did test"
    );
    assert_eq!(row["mixed_rigor"], true);
    let (_s, lb) = leaderboard_q(&app, "determinism=exact").await;
    assert_eq!(
        lb["n_rows"], 0,
        "a mixed row is not an exact-determinism row: {lb}"
    );
}

#[tokio::test]
async fn v2_digests_still_merge_under_a_v3_hub() {
    let (state, store) = setup_k(true, false, 5, 2);
    let (ka, kb) = (contributor_key(&store, "a"), contributor_key(&store, "b"));
    let app = crate::build_router(state);
    // A v2 contributor knows nothing about rigor; the bump must not orphan it.
    let (status, ack) = ingest(
        &app,
        Some(&ka),
        digest_of("anthropic", "haiku", 0.8, 100, "openai"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["accepted"], 1);
    ingest(
        &app,
        Some(&kb),
        rigor_digest("haiku", "exact", "all", "all"),
    )
    .await;
    let (_s, lb) = leaderboard(&app).await;
    let row = &lb["rows"][0];
    assert_eq!(
        row["n_contributors"], 2,
        "the v2 contribution still counts: {lb}"
    );
    // One silent source voids the claim — the row never inherits the rigorous source's badge.
    assert!(row["rigor"]["determinism"].is_null(), "{lb}");
    assert_eq!(row["rigor"]["frozen_dataset"], "mixed");
    let (_s, lb) = leaderboard_q(&app, "frozen_dataset=true").await;
    assert_eq!(
        lb["n_rows"], 0,
        "a partly-unknown row is not a frozen-dataset row: {lb}"
    );
}
