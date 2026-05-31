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
    /// Judge model, e.g. `claude-haiku-4-5`.
    pub scored_by: String,
    /// Cost of the judge call. Recorded for visibility (Agent SDK credit burn); never throttled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

/// A benchmark definition: a dataset + rubric + judge run repeatedly to track quality over time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Benchmark {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub rubric: String,
    pub judge_model: String,
    /// How to produce outputs to judge (e.g. an endpoint, a model+prompt). Free-form for now.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub target: Value,
    /// Reference to the case dataset (path/URI/table). Resolved by the runner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_ref: Option<String>,
    /// Baseline mean score to detect regressions against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_score: Option<f64>,
    pub created_at: DateTime<Utc>,
}

/// One execution of a [`Benchmark`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkRun {
    pub id: String,
    pub benchmark_id: String,
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
    pub status: String,
}
