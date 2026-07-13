//! Wire + storage + leaderboard data types for the collective network. One struct per role: what a
//! run reduces to ([`RunStat`]), what goes on the wire ([`ModelDigestEntry`] / [`CollectiveDigest`]),
//! what a hub persists ([`CollectiveEntry`]), and what the merged leaderboard exposes ([`LeaderboardRow`]).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ANON_CONTRIBUTOR, DIGEST_SCHEMA_VERSION};

/// One benchmark run reduced to the fields a digest needs. Built by the API from a `(Benchmark,
/// BenchmarkRun)` pair — only aggregate numbers + the (public) model identity + a coarse task-type
/// bucket, never any case text.
#[derive(Debug, Clone)]
pub struct RunStat {
    pub provider: String,
    pub model: String,
    pub task_type: String,
    /// Mean quality score, normalized 0..1.
    pub quality: f64,
    /// Fraction of cases that passed, 0..1.
    pub pass_rate: f64,
    /// Cost per case in USD (generation + judge).
    pub cost_per_case_usd: f64,
    pub n_cases: u32,
    pub p50_latency_ms: Option<u64>,
    pub p95_latency_ms: Option<u64>,
}

/// A published digest entry: one `(provider, model, task_type)` bucket aggregated across an instance's
/// runs. Purely aggregate — safe to share.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDigestEntry {
    pub provider: String,
    pub model: String,
    pub task_type: String,
    pub quality: f64,
    pub pass_rate: f64,
    /// Mean cost per case, USD.
    pub avg_cost_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_latency_ms: Option<u64>,
    pub n_runs: u32,
    pub n_cases: u32,
    /// v2: population variance of `quality` across the contributing runs (case-weighted). `None` when
    /// the bucket came from a single run (variance undefined) or from a v1 contributor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality_variance: Option<f64>,
}

/// A full digest an instance contributes to a hub. The `contributor_id` is **opaque** (a hash) but a
/// hub ignores it and derives identity from the presented bearer key; it stays on the wire only for
/// backward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectiveDigest {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default = "anon_contributor")]
    pub contributor_id: String,
    #[serde(default = "Utc::now")]
    pub generated_at: DateTime<Utc>,
    /// The k-anonymity floor used to build this digest (for auditability).
    #[serde(default)]
    pub min_cases: u32,
    #[serde(default)]
    pub entries: Vec<ModelDigestEntry>,
}

/// A stored, hub-side digest entry: a [`ModelDigestEntry`] tagged with its contributor + receipt time.
/// This is what `collective_entries` persists; the merge reads it back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectiveEntry {
    pub contributor_id: String,
    pub provider: String,
    pub model: String,
    pub task_type: String,
    pub quality: f64,
    pub pass_rate: f64,
    pub avg_cost_usd: f64,
    pub p50_latency_ms: Option<u64>,
    pub p95_latency_ms: Option<u64>,
    pub n_runs: u32,
    pub n_cases: u32,
    /// v2: population variance of `quality` across the contributor's runs; `None` for v1 contributors.
    pub quality_variance: Option<f64>,
    pub received_at: DateTime<Utc>,
}

/// One row of the merged public leaderboard: a `(provider, model, task_type)` aggregated across all
/// contributors.
#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardRow {
    pub provider: String,
    pub model: String,
    pub task_type: String,
    pub quality: f64,
    /// Approximate 95% CI **half-width** on `quality` (i.e. `quality ± quality_ci95`). `None` when too
    /// little of the weight carries a known variance to estimate it — an honest "insufficient variance
    /// data" marker rather than a fabricated interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality_ci95: Option<f64>,
    pub pass_rate: f64,
    pub avg_cost_usd: f64,
    /// Approximate merged p50: case-weighted mean of contributors' per-run p50s (see merge docs).
    pub p50_latency_ms: Option<u64>,
    /// Worst-observed p95 across contributors (the max, not a mean) — a conservative tail signal.
    pub p95_latency_ms: Option<u64>,
    /// `true` when the row aggregates fewer than the display floor of cases: shown, but not to be
    /// trusted as an authoritative ranking.
    pub low_confidence: bool,
    pub n_contributors: u32,
    pub n_runs: u32,
    pub n_cases: u32,
}

fn default_schema_version() -> u32 {
    DIGEST_SCHEMA_VERSION
}
fn anon_contributor() -> String {
    ANON_CONTRIBUTOR.to_string()
}
