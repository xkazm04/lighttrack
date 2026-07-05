//! Collective Model Intelligence Network — the opt-in network-effect surface.
//!
//! Three endpoints, mirroring the design in `docs/BENCHMARK_FRAMEWORK.md`:
//! - `GET  /v1/collective/digest` — build *this* instance's privacy-safe digest from its own benchmark
//!   run scorecards (admin; a preview of what it would contribute). Never reads `events`.
//! - `POST /v1/collective/ingest` — a hub receives a digest from a contributor and stores it (gated by
//!   `LIGHTTRACK_COLLECTIVE_ACCEPT`; off by default).
//! - `GET  /v1/collective/leaderboard` — the merged public leaderboard across all contributors.
//!
//! Privacy lives in `core::collective`: digests are aggregate-only and k-anonymized; the contributor
//! id is an opaque, non-reversible hash so a hub can update a source idempotently without learning who
//! it is.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use lighttrack_core::{
    build_digest, merge_leaderboard, task_type_from, Benchmark, BenchmarkRun, CollectiveDigest,
    CollectiveEntry, LeaderboardRow, RunStat, DEFAULT_MIN_CASES, DIGEST_SCHEMA_VERSION,
};
use lighttrack_store::{Store, StoreError};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::{spawn_db, AppState};

/// Collective-network config, built from env once at boot (mirrors `Alerter`/`Redactor`).
pub(crate) struct Collective {
    /// Opaque, stable contributor id put on the wire (a hash of `LIGHTTRACK_COLLECTIVE_ID`, or
    /// `anonymous` when unset). Never the raw id.
    pub(crate) contributor_id: String,
    /// Whether this instance acts as a hub that accepts contributions.
    pub(crate) accept: bool,
}

impl Collective {
    pub(crate) fn from_env() -> Self {
        let contributor_id = match std::env::var("LIGHTTRACK_COLLECTIVE_ID") {
            Ok(id) if !id.trim().is_empty() => format!("c-{}", opaque(id.trim())),
            _ => lighttrack_core::collective::ANON_CONTRIBUTOR.to_string(),
        };
        let accept = matches!(
            std::env::var("LIGHTTRACK_COLLECTIVE_ACCEPT").as_deref(),
            Ok("1") | Ok("true") | Ok("on") | Ok("yes")
        );
        Self { contributor_id, accept }
    }

    pub(crate) fn describe(&self) -> String {
        let who = if self.contributor_id == "anonymous" { "anon" } else { "id-set" };
        format!("{who}, accept={}", self.accept)
    }
}

/// First 12 hex chars of SHA-256 — opaque and non-reversible, enough to keep contributors distinct.
fn opaque(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect()
}

#[derive(Deserialize)]
pub(crate) struct DigestParams {
    /// k-anonymity floor; defaults to [`DEFAULT_MIN_CASES`]. Clamped to ≥1.
    min_cases: Option<u32>,
}

/// Build this instance's digest from every benchmark run it stores (admin-only — it walks all
/// projects). Returns what `lt collective contribute` would POST to a hub.
pub(crate) async fn get_digest(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DigestParams>,
) -> Result<Json<CollectiveDigest>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let min_cases = q.min_cases.unwrap_or(DEFAULT_MIN_CASES).max(1);

    let store = st.store.clone();
    let stats = spawn_db(move || gather_run_stats(store.as_ref())).await?;
    let entries = build_digest(&stats, min_cases);
    Ok(Json(CollectiveDigest {
        schema_version: DIGEST_SCHEMA_VERSION,
        contributor_id: st.collective.contributor_id.clone(),
        generated_at: Utc::now(),
        min_cases,
        entries,
    }))
}

#[derive(Serialize)]
pub(crate) struct IngestAck {
    contributor_id: String,
    accepted: usize,
    skipped: usize,
}

/// Hub side: accept a contributor's digest and replace its stored entry set (delete-then-upsert so a
/// bucket that fell below the floor doesn't linger). Off unless `LIGHTTRACK_COLLECTIVE_ACCEPT` is set.
pub(crate) async fn post_ingest(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(digest): Json<CollectiveDigest>,
) -> Result<Json<IngestAck>, ApiError> {
    // Honor the API's auth mode (a key in enforced mode), but no admin: contributions are public.
    authenticate(&st, &headers).await?;
    if !st.collective.accept {
        return Err(ApiError::forbidden(
            "this instance does not accept collective contributions (set LIGHTTRACK_COLLECTIVE_ACCEPT=1)",
        ));
    }
    if digest.schema_version != DIGEST_SCHEMA_VERSION {
        return Err(ApiError::bad_request(format!(
            "unsupported digest schema_version {} (this hub speaks v{DIGEST_SCHEMA_VERSION})",
            digest.schema_version
        )));
    }
    let contributor = sanitize_contributor(&digest.contributor_id);
    let now = Utc::now();
    let total = digest.entries.len();
    let entries: Vec<CollectiveEntry> = digest
        .entries
        .into_iter()
        .filter_map(|e| sanitize_entry(&contributor, e, now))
        .take(MAX_ENTRIES)
        .collect();
    let accepted = entries.len();

    let store = st.store.clone();
    let contrib = contributor.clone();
    spawn_db(move || -> Result<(), StoreError> {
        store.delete_collective_entries(&contrib)?;
        for e in &entries {
            store.upsert_collective_entry(e)?;
        }
        Ok(())
    })
    .await?;

    Ok(Json(IngestAck {
        contributor_id: contributor,
        accepted,
        skipped: total - accepted,
    }))
}

#[derive(Deserialize)]
pub(crate) struct LeaderboardParams {
    /// Filter to one task-type bucket (e.g. `qa`, `summarization`).
    task_type: Option<String>,
    /// Filter to one provider (e.g. `anthropic`).
    provider: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct LeaderboardResponse {
    /// Distinct contributing instances overall (before any task_type/provider filter).
    contributors: usize,
    n_models: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_type: Option<String>,
    rows: Vec<LeaderboardRow>,
}

/// The merged public leaderboard. Readable by anyone the API lets in (no admin) — the whole point is
/// that every operator benefits.
pub(crate) async fn get_leaderboard(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LeaderboardParams>,
) -> Result<Json<LeaderboardResponse>, ApiError> {
    authenticate(&st, &headers).await?;
    let store = st.store.clone();
    let entries = spawn_db(move || store.list_collective_entries()).await?;
    let contributors = entries
        .iter()
        .map(|e| e.contributor_id.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();

    let mut rows = merge_leaderboard(&entries);
    if let Some(tt) = q.task_type.as_deref() {
        rows.retain(|r| r.task_type == tt);
    }
    if let Some(p) = q.provider.as_deref() {
        rows.retain(|r| r.provider == p);
    }
    Ok(Json(LeaderboardResponse {
        contributors,
        n_models: rows.len(),
        task_type: q.task_type,
        rows,
    }))
}

/// Hard cap on entries accepted from one contributor, so a malformed/abusive digest can't blow up.
const MAX_ENTRIES: usize = 5000;

/// Walk every project's benchmarks and reduce each run scorecard to a [`RunStat`]. Only runs whose
/// model identity is known and that scored ≥1 case contribute — so no empty/ambiguous rows leak.
fn gather_run_stats(store: &dyn Store) -> Result<Vec<RunStat>, StoreError> {
    let mut stats = Vec::new();
    for p in store.list_projects()? {
        for b in store.list_benchmarks(&p.id)? {
            for run in store.list_benchmark_runs(&b.id)? {
                if let Some(s) = run_stat(&b, &run) {
                    stats.push(s);
                }
            }
        }
    }
    Ok(stats)
}

/// Reduce one `(Benchmark, run)` to a [`RunStat`], or `None` when it can't contribute (no known
/// provider/model, no quality, or no cases).
fn run_stat(bench: &Benchmark, run: &BenchmarkRun) -> Option<RunStat> {
    let (provider, model) = provider_model(bench, run)?;
    let quality = run.mean_score?;
    if run.n_cases == 0 {
        return None;
    }
    let cost_per_case_usd = run.cost_usd / run.n_cases as f64;
    Some(RunStat {
        provider,
        model,
        task_type: task_type_from(&bench.name, None),
        quality,
        pass_rate: run.pass_rate.unwrap_or(0.0),
        cost_per_case_usd,
        n_cases: run.n_cases,
        p50_latency_ms: run.p50_latency_ms,
        p95_latency_ms: run.p95_latency_ms,
    })
}

/// Resolve the model identity from the compare-mode run report, else the benchmark's single target.
fn provider_model(bench: &Benchmark, run: &BenchmarkRun) -> Option<(String, String)> {
    let from = |v: &Value| {
        let p = v.get("provider").and_then(Value::as_str)?.trim().to_string();
        let m = v.get("model").and_then(Value::as_str)?.trim().to_string();
        (!p.is_empty() && !m.is_empty()).then_some((p, m))
    };
    from(&run.report).or_else(|| from(&bench.target))
}

fn sanitize_contributor(id: &str) -> String {
    let id = id.trim();
    if id.is_empty() {
        lighttrack_core::collective::ANON_CONTRIBUTOR.to_string()
    } else {
        // Keep it opaque + bounded; ids are already hashes from the contributor, but be defensive.
        id.chars().take(64).collect()
    }
}

/// Validate/clamp one contributed entry; `None` if it lacks a usable model identity.
fn sanitize_entry(
    contributor: &str,
    e: lighttrack_core::ModelDigestEntry,
    now: chrono::DateTime<Utc>,
) -> Option<CollectiveEntry> {
    let provider = e.provider.trim().to_string();
    let model = e.model.trim().to_string();
    let task_type = e.task_type.trim().to_string();
    if provider.is_empty() || model.is_empty() || task_type.is_empty() || e.n_cases == 0 {
        return None;
    }
    Some(CollectiveEntry {
        contributor_id: contributor.to_string(),
        provider,
        model,
        task_type,
        quality: e.quality.clamp(0.0, 1.0),
        pass_rate: e.pass_rate.clamp(0.0, 1.0),
        avg_cost_usd: e.avg_cost_usd.max(0.0),
        p50_latency_ms: e.p50_latency_ms,
        p95_latency_ms: e.p95_latency_ms,
        n_runs: e.n_runs,
        n_cases: e.n_cases,
        received_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bench(name: &str, target: Value) -> Benchmark {
        Benchmark {
            id: "b1".into(),
            project_id: "p1".into(),
            name: name.into(),
            rubric: String::new(),
            judge_model: "haiku".into(),
            target,
            dataset_ref: None,
            rubric_id: None,
            dataset: vec![],
            baseline_score: None,
            created_at: Utc::now(),
        }
    }

    fn run(report: Value, mean: Option<f64>, cases: u32, cost: f64) -> BenchmarkRun {
        BenchmarkRun {
            id: "r1".into(),
            benchmark_id: "b1".into(),
            started_at: Utc::now(),
            finished_at: None,
            n_cases: cases,
            mean_score: mean,
            pass_rate: Some(0.8),
            cost_usd: cost,
            status: "compared".into(),
            p50_latency_ms: Some(700),
            p95_latency_ms: Some(1400),
            total_tokens: Some(1000),
            report,
        }
    }

    #[test]
    fn run_stat_reads_compare_report() {
        let b = bench("Nightly QA bench", Value::Null);
        let r = run(json!({"provider":"anthropic","model":"haiku"}), Some(0.82), 20, 0.4);
        let s = run_stat(&b, &r).unwrap();
        assert_eq!((s.provider.as_str(), s.model.as_str()), ("anthropic", "haiku"));
        assert_eq!(s.task_type, "qa");
        assert!((s.cost_per_case_usd - 0.02).abs() < 1e-9); // 0.4 / 20
    }

    #[test]
    fn run_stat_falls_back_to_target_then_skips() {
        // No report identity, but the benchmark's single target carries it.
        let b = bench("Summaries", json!({"provider":"openai","model":"gpt-x"}));
        let r = run(Value::Null, Some(0.7), 10, 0.1);
        let s = run_stat(&b, &r).unwrap();
        assert_eq!(s.model, "gpt-x");
        assert_eq!(s.task_type, "summarization");
        // No identity anywhere → skipped.
        let b2 = bench("x", Value::Null);
        assert!(run_stat(&b2, &run(Value::Null, Some(0.7), 10, 0.1)).is_none());
        // No quality → skipped.
        assert!(run_stat(&b, &run(json!({"provider":"a","model":"m"}), None, 10, 0.1)).is_none());
    }

    #[test]
    fn opaque_id_is_stable_and_not_the_input() {
        let a = opaque("my-secret-instance-id");
        assert_eq!(a, opaque("my-secret-instance-id"));
        assert_ne!(a, "my-secret-instance-id");
        assert_eq!(a.len(), 12);
    }

    #[test]
    fn sanitize_entry_clamps_and_drops_identityless() {
        let now = Utc::now();
        let good = lighttrack_core::ModelDigestEntry {
            provider: "anthropic".into(), model: "haiku".into(), task_type: "qa".into(),
            quality: 1.4, pass_rate: -0.2, avg_cost_usd: -1.0,
            p50_latency_ms: None, p95_latency_ms: None, n_runs: 2, n_cases: 9,
        };
        let s = sanitize_entry("c-abc", good, now).unwrap();
        assert_eq!(s.quality, 1.0);
        assert_eq!(s.pass_rate, 0.0);
        assert_eq!(s.avg_cost_usd, 0.0);
        let bad = lighttrack_core::ModelDigestEntry {
            provider: "  ".into(), model: "haiku".into(), task_type: "qa".into(),
            quality: 0.5, pass_rate: 0.5, avg_cost_usd: 0.1,
            p50_latency_ms: None, p95_latency_ms: None, n_runs: 1, n_cases: 5,
        };
        assert!(sanitize_entry("c-abc", bad, now).is_none());
    }
}
