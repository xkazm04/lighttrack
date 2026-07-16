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
    /// Coarse judge family that scored this run (`anthropic|openai|google|unknown`), or `None` when the
    /// benchmark records no judge. Provider only — never the full judge model — to limit fingerprinting.
    pub judge_provider: Option<String>,
    /// Short, one-way hash of the rubric shape/criteria — lets the hub tell whether two numbers were
    /// scored under the same rubric without ever seeing the rubric text.
    pub rubric_fingerprint: Option<String>,
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
    /// v2: coarse judge family for this bucket (`anthropic|openai|google|unknown`, or `mixed` when the
    /// bucket's runs disagree). Provider only — never the full judge model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_provider: Option<String>,
    /// v2: rubric-shape fingerprint (short one-way hash). `None` when the bucket mixes rubrics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubric_fingerprint: Option<String>,
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
    /// Consent envelope: how many projects opted into this digest, and how many were withheld for
    /// lacking `collective_opt_in`. Makes what leaves the building legible *before* the POST.
    /// Serde-defaulted so v1/v2 hubs (which ignore unknown fields) stay wire-compatible.
    #[serde(default)]
    pub projects_included: u32,
    #[serde(default)]
    pub projects_excluded: u32,
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
    /// v2: coarse judge family (`anthropic|openai|google|unknown|mixed`) or `None` (v1 / unrecorded).
    pub judge_provider: Option<String>,
    /// v2: rubric-shape fingerprint (short one-way hash) or `None`.
    pub rubric_fingerprint: Option<String>,
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
    /// Distinct coarse judge families behind this row (sorted). Cross-instance quality is only
    /// commensurable when these agree — the row is judged by whatever scored each contribution.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub judge_providers: Vec<String>,
    /// `Some(n)` when more than one distinct judge family contributed — the number is incommensurable
    /// across judges, so treat the ranking with care. `None` when a single judge (or none recorded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mixed_judges: Option<u32>,
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
