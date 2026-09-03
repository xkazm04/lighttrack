//! Wire + storage + leaderboard data types for the collective network. One struct per role: what a
//! run reduces to ([`RunStat`]), what goes on the wire ([`ModelDigestEntry`] / [`CollectiveDigest`]),
//! what a hub persists ([`CollectiveEntry`]), and what the merged leaderboard exposes ([`LeaderboardRow`]).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::rigor::{Coverage, RowRigor};
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
    /// v3 rigor: the run's weakest determinism stamp (`exact` | `best-effort` | `sampled`), or `None`
    /// when the run recorded none.
    pub determinism: Option<String>,
    /// v3 rigor: whether this run's cases came from a **frozen** dataset. `None` when the run recorded
    /// no dataset provenance (an inline case list, or a dataset whose head could not be read).
    pub dataset_frozen: Option<bool>,
    /// v3 rigor: the pinned dataset version. Consumed by [`build_digest`](super::build_digest) to tell
    /// whether a bucket's runs all sat on **one** pin — the integer itself is never published (see the
    /// fingerprinting argument in [`rigor`](super::rigor)).
    pub dataset_version: Option<u32>,
    /// v3 rigor: whether the run's verdict carried an interval (`n ≥ 2` + a `ci95`) rather than a bare
    /// point estimate. `None` when the run recorded no significance annotation at all.
    pub significance_tested: Option<bool>,
}

/// A published digest entry: one `(provider, model, task_type)` bucket aggregated across an instance's
/// runs. Purely aggregate — safe to share.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// v3: the **weakest** determinism stamp across the bucket's runs — a bucket is only as
    /// reproducible as its least reproducible run. `None` when any run recorded none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub determinism: Option<String>,
    /// v3: whether the bucket's runs all ran against a frozen dataset pinned at a **single** version.
    /// `all` is a complete claim; `mixed` means the runs disagreed (or drifted across versions);
    /// `unknown` means no run recorded dataset provenance.
    #[serde(default, skip_serializing_if = "Coverage::is_unknown")]
    pub frozen_dataset: Coverage,
    /// v3: whether the bucket's runs all carried a significance-tested verdict.
    #[serde(default, skip_serializing_if = "Coverage::is_unknown")]
    pub significance_tested: Coverage,
}

/// A full digest an instance contributes to a hub. The `contributor_id` is **opaque** (a hash) but a
/// hub ignores it and derives identity from the presented bearer key; it stays on the wire only for
/// backward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// v3: weakest determinism stamp behind this contribution (`None` for v1/v2 contributors).
    #[serde(default)]
    pub determinism: Option<String>,
    /// v3: frozen+single-version dataset coverage (`Unknown` for v1/v2 contributors).
    #[serde(default)]
    pub frozen_dataset: Coverage,
    /// v3: significance-tested-verdict coverage (`Unknown` for v1/v2 contributors).
    #[serde(default)]
    pub significance_tested: Coverage,
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
    /// Approximate 95% CI **half-width** on `quality` (i.e. `quality ± quality_ci95`), combining the
    /// pooled within-source case variance with a random-effects **between-source** term — so
    /// contributors who disagree widen the interval instead of hiding in it. `None` when too little of
    /// the weight carries a known variance to estimate the within term — an honest "insufficient
    /// variance data" marker rather than a fabricated interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality_ci95: Option<f64>,
    /// Weighted standard deviation of the **per-source** mean qualities behind this row — how much the
    /// contributors disagree, published even when no CI could be formed. `None` for a single-source
    /// row: with one source there is no between-source evidence, which is not the same as no
    /// disagreement. At two sources it rests on one degree of freedom — read it as a lower bound (see
    /// the `spread` module).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_spread: Option<f64>,
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
    /// `Some(n)` when more than one judge family stands behind the row — the number is incommensurable
    /// across judges, so treat the ranking with care. A source tagged `mixed` counts as two families,
    /// so a row built from one such source is flagged too. `None` when a single judge (or none
    /// recorded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mixed_judges: Option<u32>,
    pub n_contributors: u32,
    pub n_runs: u32,
    pub n_cases: u32,
    /// Share of this row's **effective** (winsorized) weight held by its single largest source, in
    /// `(0, 1]`. Provenance, not decoration: `1.0` means one instance's private eval results, and the
    /// merge caps this at [`MAX_SOURCE_WEIGHT_SHARE`](super::MAX_SOURCE_WEIGHT_SHARE) whenever the row
    /// has ≥2 sources, so no contributor can own a row outright however many cases it claims.
    pub max_source_share: f64,
    /// v3: how rigorous the evidence behind this row was — determinism, frozen datasets, whether the
    /// verdicts were significance-tested. Mixture is disclosed, not averaged away: see
    /// [`RowRigor`](super::rigor::RowRigor).
    pub rigor: RowRigor,
    /// `true` when the row's sources disagree on any rigor facet. A convenience mirror of
    /// [`RowRigor::is_mixed`] so a renderer/filter doesn't have to re-derive it.
    pub mixed_rigor: bool,
}

fn default_schema_version() -> u32 {
    DIGEST_SCHEMA_VERSION
}
fn anon_contributor() -> String {
    ANON_CONTRIBUTOR.to_string()
}
