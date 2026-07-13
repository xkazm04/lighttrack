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
    CollectiveEntry, LeaderboardRow, ModelAliases, RunStat, DEFAULT_LOW_CONFIDENCE_CASES,
    DEFAULT_MIN_CASES, DIGEST_SCHEMA_VERSION, MIN_SCHEMA_VERSION,
};
use lighttrack_store::{Store, StoreError};

use crate::error::ApiError;
use crate::guards::{authenticate, bearer, ensure_can_admin};
use crate::state::{spawn_db, AppState};

/// Collective-network config, built from env once at boot (mirrors `Alerter`/`Redactor`).
pub(crate) struct Collective {
    /// Opaque, stable id this instance stamps on its *own* digest preview (a hash of
    /// `LIGHTTRACK_COLLECTIVE_ID`, or `anonymous` when unset). Never the raw id. NB: a hub **ignores**
    /// this on ingest and derives the identity from the presented bearer key — see [`post_ingest`].
    pub(crate) contributor_id: String,
    /// Whether this instance acts as a hub that accepts contributions.
    pub(crate) accept: bool,
    /// Hub-side: accept anonymous (keyless) contributions under a single shared `anonymous` identity.
    /// Off by default — a keyless push is refused so one poster can't masquerade as many.
    pub(crate) allow_anon: bool,
    /// Hub-side k-anonymity floor: buckets contributed with `n_cases` below this are dropped on ingest,
    /// regardless of what floor the contributor claims to have used. Clamped to ≥1.
    pub(crate) min_cases: u32,
    /// Leaderboard display floor: merged rows with fewer than this many total cases are flagged
    /// `low_confidence` (shown, not hidden).
    pub(crate) display_floor: u32,
    /// Model-identity normalization applied to `(provider, model)` at ingest, so `gpt-4o` /
    /// `openai/gpt-4o` / `gpt-4o-2024-08-06` collapse to one leaderboard row. Empty ⇒ pass-through.
    pub(crate) aliases: ModelAliases,
}

impl Collective {
    pub(crate) fn from_env() -> Self {
        let contributor_id = match std::env::var("LIGHTTRACK_COLLECTIVE_ID") {
            Ok(id) if !id.trim().is_empty() => format!("c-{}", opaque(id.trim())),
            _ => lighttrack_core::collective::ANON_CONTRIBUTOR.to_string(),
        };
        let accept = env_flag("LIGHTTRACK_COLLECTIVE_ACCEPT");
        let allow_anon = env_flag("LIGHTTRACK_COLLECTIVE_ALLOW_ANON");
        let min_cases = std::env::var("LIGHTTRACK_COLLECTIVE_MIN_CASES")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(DEFAULT_MIN_CASES)
            .max(1);
        let display_floor = std::env::var("LIGHTTRACK_COLLECTIVE_DISPLAY_FLOOR")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(DEFAULT_LOW_CONFIDENCE_CASES);
        let aliases = load_aliases();
        Self { contributor_id, accept, allow_anon, min_cases, display_floor, aliases }
    }

    pub(crate) fn describe(&self) -> String {
        let who = if self.contributor_id == "anonymous" { "anon" } else { "id-set" };
        format!(
            "{who}, accept={}, allow_anon={}, min_cases={}, display_floor={}",
            self.accept, self.allow_anon, self.min_cases, self.display_floor
        )
    }
}

fn env_flag(name: &str) -> bool {
    matches!(std::env::var(name).as_deref(), Ok("1") | Ok("true") | Ok("on") | Ok("yes"))
}

/// Load the model-alias table from `LIGHTTRACK_MODEL_ALIASES` (default `config/model_aliases.json`).
/// Absent ⇒ an empty (pass-through) table; a parse error is logged and normalization is disabled.
fn load_aliases() -> ModelAliases {
    let path = std::env::var("LIGHTTRACK_MODEL_ALIASES")
        .unwrap_or_else(|_| "config/model_aliases.json".to_string());
    match std::fs::read_to_string(&path) {
        Ok(s) => ModelAliases::from_json_str(&s).unwrap_or_else(|e| {
            eprintln!("model aliases parse error in {path}: {e}; normalization disabled");
            ModelAliases::default()
        }),
        Err(_) => ModelAliases::default(),
    }
}

/// First 12 hex chars of SHA-256 — opaque and non-reversible, enough to keep contributors distinct.
fn opaque(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// Derive a hub-side contributor id from the raw bearer token the poster presented: `c-` + the first
/// 12 hex of SHA-256(token). The id is **not** taken from the request body — a poster can only write
/// under the identity of a key it actually holds, so it can neither overwrite a victim's set nor forge
/// unlimited ids to inflate `n_contributors`.
fn derive_contributor_id(bearer: &str) -> String {
    format!("c-{}", opaque(bearer))
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
    /// The **hub-derived** identity this contribution landed under (from the bearer key, not the body).
    contributor_id: String,
    accepted: usize,
    /// Entries dropped as malformed / identity-less (empty provider, model, or task_type).
    skipped: usize,
    /// Entries dropped for failing the hub's enforced k-anonymity floor (`n_cases < min_cases`).
    dropped_under_min: usize,
}

/// Hub side: accept a contributor's digest and replace its stored entry set (delete-then-upsert so a
/// bucket that fell below the floor doesn't linger). Off unless `LIGHTTRACK_COLLECTIVE_ACCEPT` is set.
///
/// Hardening: the contributor identity is **derived from the presented bearer key**, never trusted from
/// the request body — so a poster can only ever replace *its own* set. A keyless (dev-mode) push is
/// refused unless `LIGHTTRACK_COLLECTIVE_ALLOW_ANON=1`, in which case it lands under one shared
/// `anonymous` identity with a loud warning. The hub also re-enforces its own k-anonymity floor
/// (`LIGHTTRACK_COLLECTIVE_MIN_CASES`), dropping under-k buckets rather than trusting the poster's floor.
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
    if !(MIN_SCHEMA_VERSION..=DIGEST_SCHEMA_VERSION).contains(&digest.schema_version) {
        return Err(ApiError::bad_request(format!(
            "unsupported digest schema_version {} (this hub accepts v{MIN_SCHEMA_VERSION}..=v{DIGEST_SCHEMA_VERSION})",
            digest.schema_version
        )));
    }

    // Derive identity from the bearer key; the body's `contributor_id` is ignored (kept for wire compat).
    let contributor = match bearer(&headers) {
        Some(tok) => derive_contributor_id(&tok),
        None => {
            if !st.collective.allow_anon {
                return Err(ApiError::forbidden(
                    "anonymous (keyless) contributions are refused; present a bearer key, or set \
                     LIGHTTRACK_COLLECTIVE_ALLOW_ANON=1 to accept them under one shared identity",
                ));
            }
            eprintln!(
                "WARNING: accepting an ANONYMOUS collective contribution (LIGHTTRACK_COLLECTIVE_ALLOW_ANON=1) \
                 — all keyless posters share the '{}' identity and can overwrite each other's set",
                lighttrack_core::collective::ANON_CONTRIBUTOR
            );
            lighttrack_core::collective::ANON_CONTRIBUTOR.to_string()
        }
    };

    let min_cases = st.collective.min_cases;
    let now = Utc::now();
    let mut skipped = 0usize;
    let mut dropped_under_min = 0usize;
    let entries: Vec<CollectiveEntry> = digest
        .entries
        .into_iter()
        .filter_map(|e| match sanitize_entry(&contributor, e, now, &st.collective.aliases) {
            None => {
                skipped += 1;
                None
            }
            Some(ce) if ce.n_cases < min_cases => {
                dropped_under_min += 1;
                None
            }
            Some(ce) => Some(ce),
        })
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

    Ok(Json(IngestAck { contributor_id: contributor, accepted, skipped, dropped_under_min }))
}

#[derive(Deserialize)]
pub(crate) struct LeaderboardParams {
    /// Filter to one task-type bucket (e.g. `qa`, `summarization`).
    task_type: Option<String>,
    /// Filter to one provider (e.g. `anthropic`).
    provider: Option<String>,
    /// Filter to rows scored (at least partly) by one judge family (`anthropic|openai|google|unknown`).
    judge: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct LeaderboardResponse {
    /// Distinct contributing instances **backing the visible rows** — computed over the filtered row
    /// set, so it never disagrees with what's shown. A filter that excludes a contributor's only rows
    /// drops it from this count.
    contributors: usize,
    /// Distinct `(provider, model)` identities across the filtered rows — a true model count, not a row
    /// count. (A single model spans multiple rows when it appears under several task types.)
    n_models: usize,
    /// Number of visible leaderboard rows after filtering (one per `(provider, model, task_type)`).
    n_rows: usize,
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

    let mut rows = merge_leaderboard(&entries, st.collective.display_floor);
    if let Some(tt) = q.task_type.as_deref() {
        rows.retain(|r| r.task_type == tt);
    }
    if let Some(p) = q.provider.as_deref() {
        rows.retain(|r| r.provider == p);
    }
    if let Some(j) = q.judge.as_deref() {
        rows.retain(|r| r.judge_providers.iter().any(|p| p == j));
    }

    // Header counts are computed over the FILTERED rows so they never disagree with what's shown.
    // Contributors backing the visible rows = distinct contributor ids of every stored entry whose
    // `(provider, model, task_type)` survived filtering (an entry's identity is normalized at ingest,
    // so its key matches the merged row's key exactly).
    let surviving: std::collections::BTreeSet<(&str, &str, &str)> = rows
        .iter()
        .map(|r| (r.provider.as_str(), r.model.as_str(), r.task_type.as_str()))
        .collect();
    let contributors = entries
        .iter()
        .filter(|e| surviving.contains(&(e.provider.as_str(), e.model.as_str(), e.task_type.as_str())))
        .map(|e| e.contributor_id.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let n_models = rows
        .iter()
        .map(|r| (r.provider.as_str(), r.model.as_str()))
        .collect::<std::collections::BTreeSet<_>>()
        .len();

    Ok(Json(LeaderboardResponse {
        contributors,
        n_models,
        n_rows: rows.len(),
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
        judge_provider: judge_provider_of(&bench.judge_model),
        rubric_fingerprint: rubric_fingerprint_of(bench),
    })
}

/// Classify a benchmark's `judge_model` (`[provider/]model`) into a coarse judge family — provider
/// only (`anthropic|openai|google|unknown`), never the full model, to limit fingerprinting. An
/// explicit `provider/` prefix wins; otherwise the family is inferred from the model name.
fn judge_provider_of(judge_model: &str) -> Option<String> {
    let m = judge_model.trim().to_lowercase();
    if m.is_empty() {
        return None;
    }
    let (prefix, name) = m.split_once('/').unwrap_or(("", m.as_str()));
    let canon_prefix = match prefix {
        "anthropic" | "claude" => Some("anthropic"),
        "openai" | "azure-openai" | "azure" => Some("openai"),
        "google" | "gemini" | "vertex" | "google-vertex" => Some("google"),
        _ => None,
    };
    if let Some(c) = canon_prefix {
        return Some(c.to_string());
    }
    let name = if name.is_empty() { m.as_str() } else { name };
    let family = if ["claude", "haiku", "sonnet", "opus"].iter().any(|k| name.contains(k)) {
        "anthropic"
    } else if name.contains("gpt") || name.starts_with("o1") || name.starts_with("o3") {
        "openai"
    } else if name.contains("gemini") || name.contains("gemma") || name.contains("bison") {
        "google"
    } else {
        "unknown"
    };
    Some(family.to_string())
}

/// A short, one-way fingerprint of a benchmark's rubric shape — 8 hex of SHA-256 over the
/// whitespace-normalized rubric definition (or its id, if the text is empty). Lets two instances tell
/// whether they scored under the same rubric without either revealing the rubric text. `None` when the
/// benchmark carries no rubric at all.
fn rubric_fingerprint_of(bench: &Benchmark) -> Option<String> {
    let basis = if !bench.rubric.trim().is_empty() {
        bench.rubric.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        bench.rubric_id.as_deref().map(str::trim).filter(|s| !s.is_empty())?.to_string()
    };
    let mut h = Sha256::new();
    h.update(basis.as_bytes());
    Some(h.finalize().iter().take(4).map(|b| format!("{b:02x}")).collect())
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

/// Validate/clamp one contributed entry; `None` if it lacks a usable model identity. The model
/// identity is **normalized** through `aliases` so equivalent spellings merge into one leaderboard row.
fn sanitize_entry(
    contributor: &str,
    e: lighttrack_core::ModelDigestEntry,
    now: chrono::DateTime<Utc>,
    aliases: &ModelAliases,
) -> Option<CollectiveEntry> {
    let provider = e.provider.trim();
    let model = e.model.trim();
    let task_type = e.task_type.trim().to_string();
    if provider.is_empty() || model.is_empty() || task_type.is_empty() || e.n_cases == 0 {
        return None;
    }
    let (provider, model) = aliases.normalize(provider, model);
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
        // v2: carry the variance if present; a negative value is nonsense, so drop it to None.
        quality_variance: e.quality_variance.filter(|v| v.is_finite() && *v >= 0.0),
        // v2: clamp the judge tag to the known vocabulary; anything else is `unknown`.
        judge_provider: e
            .judge_provider
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(canon_judge),
        rubric_fingerprint: e
            .rubric_fingerprint
            .map(|r| r.trim().chars().take(32).collect::<String>())
            .filter(|s| !s.is_empty()),
        received_at: now,
    })
}

/// Clamp a contributed judge tag to the known vocabulary (`anthropic|openai|google|mixed`), mapping
/// anything unrecognized to `unknown` so a poster can't inject arbitrary judge labels.
fn canon_judge(j: &str) -> String {
    match j.to_lowercase().as_str() {
        "anthropic" => "anthropic",
        "openai" => "openai",
        "google" => "google",
        "mixed" => "mixed",
        _ => "unknown",
    }
    .to_string()
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
        let a = ModelAliases::default();
        let good = lighttrack_core::ModelDigestEntry {
            provider: "anthropic".into(), model: "haiku".into(), task_type: "qa".into(),
            quality: 1.4, pass_rate: -0.2, avg_cost_usd: -1.0,
            p50_latency_ms: None, p95_latency_ms: None, n_runs: 2, n_cases: 9,
            quality_variance: Some(-0.5), // negative variance is nonsense → dropped to None
            judge_provider: Some("weird-label".into()), // unknown label → clamped to "unknown"
            rubric_fingerprint: Some("ab12cd34".into()),
        };
        let s = sanitize_entry("c-abc", good, now, &a).unwrap();
        assert_eq!(s.quality, 1.0);
        assert_eq!(s.pass_rate, 0.0);
        assert_eq!(s.avg_cost_usd, 0.0);
        assert!(s.quality_variance.is_none(), "negative variance dropped");
        assert_eq!(s.judge_provider.as_deref(), Some("unknown"), "unknown judge label clamped");
        assert_eq!(s.rubric_fingerprint.as_deref(), Some("ab12cd34"));
        let bad = lighttrack_core::ModelDigestEntry {
            provider: "  ".into(), model: "haiku".into(), task_type: "qa".into(),
            quality: 0.5, pass_rate: 0.5, avg_cost_usd: 0.1,
            p50_latency_ms: None, p95_latency_ms: None, n_runs: 1, n_cases: 5,
            quality_variance: None, judge_provider: None, rubric_fingerprint: None,
        };
        assert!(sanitize_entry("c-abc", bad, now, &a).is_none());
    }

    #[test]
    fn ingest_normalizes_model_identity() {
        let now = Utc::now();
        let a = ModelAliases::from_json_str(
            r#"{"providers":{"azure-openai":"openai"},"models":{"gpt-4o-2024-08-06":"gpt-4o"}}"#,
        )
        .unwrap();
        let e = |provider: &str, model: &str| lighttrack_core::ModelDigestEntry {
            provider: provider.into(), model: model.into(), task_type: "qa".into(),
            quality: 0.8, pass_rate: 0.8, avg_cost_usd: 0.01,
            p50_latency_ms: None, p95_latency_ms: None, n_runs: 1, n_cases: 10,
            quality_variance: None, judge_provider: None, rubric_fingerprint: None,
        };
        // provider/ prefix stripped + dated variant collapsed + provider synonym mapped.
        let s = sanitize_entry("c", e("openai", "openai/gpt-4o-2024-08-06"), now, &a).unwrap();
        assert_eq!((s.provider.as_str(), s.model.as_str()), ("openai", "gpt-4o"));
        let s = sanitize_entry("c", e("azure-openai", "gpt-4o"), now, &a).unwrap();
        assert_eq!(s.provider, "openai");
    }

    #[test]
    fn judge_provider_classification() {
        assert_eq!(judge_provider_of("anthropic/claude-haiku-4-5").as_deref(), Some("anthropic"));
        assert_eq!(judge_provider_of("haiku").as_deref(), Some("anthropic"));
        assert_eq!(judge_provider_of("gpt-4o").as_deref(), Some("openai"));
        assert_eq!(judge_provider_of("openai/o3-mini").as_deref(), Some("openai"));
        assert_eq!(judge_provider_of("gemini-1.5-pro").as_deref(), Some("google"));
        assert_eq!(judge_provider_of("some-local-llm").as_deref(), Some("unknown"));
        assert_eq!(judge_provider_of("  "), None);
    }
}
