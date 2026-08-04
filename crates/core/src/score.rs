use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The structured verdict an LLM judge returns. Used as the `--json-schema` for `claude -p`
/// (lands in the `structured_output` field of the JSON envelope).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeVerdict {
    pub score: f64,
    #[serde(default = "one")]
    pub max: f64,
    #[serde(default)]
    pub pass: bool,
    #[serde(default)]
    pub reasoning: String,
}

fn one() -> f64 {
    1.0
}

/// JSON Schema for [`JudgeVerdict`], to pass to `claude -p --json-schema`.
pub fn judge_verdict_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "score":     { "type": "number", "description": "rubric score for this output" },
            "max":       { "type": "number", "description": "upper bound of the scale" },
            "pass":      { "type": "boolean", "description": "whether the output meets the bar" },
            "reasoning": { "type": "string", "description": "concise justification" }
        },
        "required": ["score", "max", "pass", "reasoning"],
        "additionalProperties": false
    })
}

/// Storage bounds for [`ScoreDetail`]. A score row is hot (listed, joined, alerted on), so the
/// provenance it carries is capped rather than unbounded: at most [`MAX_REASONING_CHARS`] per
/// reasoning string, [`MAX_REASONINGS_PER_DIM`] retained per dimension (the first k samples, in
/// sample order), [`MAX_DIMENSIONS`] dimensions and [`MAX_NOTES`] notes. Truncated strings end in `…`
/// so a reader can tell. Worst case ≈ 32 × 8 × 600 B ≈ 150 KB of JSON, and typical rubrics
/// (4 dims × 3 samples) land near 5 KB.
pub const MAX_REASONING_CHARS: usize = 600;
pub const MAX_REASONINGS_PER_DIM: usize = 8;
pub const MAX_DIMENSIONS: usize = 32;
pub const MAX_NOTES: usize = 8;

/// One dimension's contribution to a judged verdict, kept so a stored score can answer *why*.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScoreDim {
    pub key: String,
    /// The aggregated (mean-over-samples) dimension score.
    pub value: f64,
    pub weight: f64,
    /// The rubric's gating floor for this dimension, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<f64>,
    /// True when `value` fell below `floor` — the reason a high overall can still fail.
    #[serde(default)]
    pub floor_hit: bool,
    /// The judge's reasoning, one entry per sample that parsed (sample order). Every sample is kept:
    /// its reasoning tokens were paid for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning: Vec<String>,
}

/// Structured provenance for a judged verdict: the per-dimension breakdown plus the reliability
/// signals (agreement, sample accounting, bias/injection flags) that produced the scalar `value` on
/// [`Score`]. Additive and nullable — a score posted without it is still a valid score.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScoreDetail {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dimensions: Vec<ScoreDim>,
    /// Cross-sample agreement on the overall score (1.0 = the samples agreed exactly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agreement: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples_requested: Option<u32>,
    /// How many of the requested samples yielded a usable verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples_parsed: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse_failures: Option<u32>,
    /// Pairwise only: swapping A/B flipped the winner, so the verdict collapsed to a tie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_bias: Option<bool>,
    /// The judged content imitated a prompt boundary and was neutralized (see engine `fence`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub injection_suspected: Option<bool>,
    /// `"exact"` when every sampling control the judge provider exposes was pinned (temperature 0 +
    /// a fixed seed); `"best-effort"` when the path exposed no seed, no knobs at all, or rejected
    /// the ones we asked for. On a best-effort verdict, cross-sample agreement partly measures
    /// sampling noise rather than genuine ambiguity — so the number means less.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub determinism: Option<String>,
    /// Judge text not tied to a dimension (a freeform verdict's rationale, a pairwise rationale).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// For a **whole-trace** verdict: what it judged (span count + the fingerprint of the judged
    /// root exchange). Stamped by the API on `POST /v1/traces/:id/score`; a trace read compares it
    /// against the trace as it now stands so a verdict that stopped describing its trace says so
    /// instead of aging silently. `None` on every non-trace score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<crate::trace::TraceCoverage>,
}

/// Truncate on a char boundary, marking that it happened.
fn cap_str(s: &str) -> String {
    if s.chars().count() <= MAX_REASONING_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_REASONING_CHARS - 1).collect();
    out.push('…');
    out
}

impl ScoreDetail {
    /// Enforce the storage bounds. Applied by the API on every insert, so a client that posts an
    /// unbounded detail cannot balloon a hot score row.
    pub fn capped(mut self) -> Self {
        self.dimensions.truncate(MAX_DIMENSIONS);
        for d in &mut self.dimensions {
            d.reasoning.truncate(MAX_REASONINGS_PER_DIM);
            for r in &mut d.reasoning {
                *r = cap_str(r);
            }
        }
        self.notes.truncate(MAX_NOTES);
        for n in &mut self.notes {
            *n = cap_str(n);
        }
        self
    }

    /// True when nothing meaningful is recorded (so callers can store `None` instead of `{}`).
    pub fn is_empty(&self) -> bool {
        *self == ScoreDetail::default()
    }
}

/// A stored judge result, optionally tied to the event it scored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Score {
    #[serde(default = "crate::new_id")]
    pub id: String,
    /// Defaulted so a keyed poster can omit it (the API derives it from the API key).
    #[serde(default)]
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    pub rubric: String,
    pub value: f64,
    #[serde(default = "one")]
    pub max: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Structured provenance for this verdict (per-dimension breakdown, agreement, sample
    /// accounting, bias flags). Additive and nullable: pre-existing clients that never send or read
    /// it keep working. Persisted by the SQLite backend; other backends default it to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ScoreDetail>,
    /// The [`BenchmarkRun`] this verdict belongs to, when it came from a benchmark. Without it, two
    /// runs of the same benchmark are distinguishable only by `created_at` ordering, so "why did run
    /// 47 fail?" has no query. The runner mints the run id **before** judging and stamps it on every
    /// case it posts, so the run's cases exist even if the run row is never written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// 1-based position of the case within the run's dataset (pairwise: the case a game was played
    /// on, so it repeats across that case's games). Ordering key for the run's case listing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_index: Option<u32>,
    /// Judge model, e.g. `claude-haiku-4-5`.
    pub scored_by: String,
    /// Cost of the judge call. Recorded for visibility (Agent SDK credit burn); never throttled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

/// One target in a comparison matrix: a provider+model, optionally with a named system-prompt variant.
/// Stored inline in a benchmark's `target` field as an array (Phase 3.6e).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchTarget {
    pub provider: String,
    pub model: String,
    /// System/instruction prompt variant under test.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Display label; defaults to `provider/model` if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// One case in a benchmark dataset. `output` is the candidate to judge; `expected` is an optional
/// reference answer the judge can compare against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkCase {
    pub input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// A benchmark definition: a dataset + rubric + judge run repeatedly to track quality over time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Benchmark {
    #[serde(default = "crate::new_id")]
    pub id: String,
    #[serde(default)]
    pub project_id: String,
    pub name: String,
    pub rubric: String,
    #[serde(default = "default_judge_model")]
    pub judge_model: String,
    /// How to produce outputs to judge (e.g. an endpoint, a model+prompt). Free-form for now.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub target: Value,
    /// Reference to an external case dataset (path/URI/table), if not inlined in `dataset`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_ref: Option<String>,
    /// Optional structured rubric (id) for per-dimension judging; falls back to `rubric` text if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubric_id: Option<String>,
    /// Inline dataset of cases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dataset: Vec<BenchmarkCase>,
    /// Baseline mean score to detect regressions against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_score: Option<f64>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

fn default_judge_model() -> String {
    "haiku".to_string()
}

/// One execution of a [`Benchmark`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkRun {
    #[serde(default = "crate::new_id")]
    pub id: String,
    pub benchmark_id: String,
    #[serde(default = "Utc::now")]
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub n_cases: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_rate: Option<f64>,
    #[serde(default)]
    pub cost_usd: f64,
    /// `running` | `passed` | `regressed` | `failed`.
    #[serde(default = "default_run_status")]
    pub status: String,
    // Phase 3.6a: response-time + token aggregates for the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Phase 3.6c: per-dimension breakdown + recommendations/healing for this run.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub report: Value,
}

fn default_run_status() -> String {
    "completed".to_string()
}
