use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A named entry in the prompt registry. Holds the label→version pointers (e.g.
/// `{"production": 2, "staging": 5}`) fetched at runtime, plus an optional linked benchmark whose
/// regression check gates promotion. The actual prompt text lives in [`PromptVersion`] rows, one per
/// immutable version — so a registry edit is a new version, never an in-place overwrite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    #[serde(default = "crate::new_id")]
    pub id: String,
    #[serde(default)]
    pub project_id: String,
    /// Registry name, unique per project (e.g. `support-reply`).
    pub name: String,
    /// Benchmark run on each new version; its regression verdict gates label promotion. Reuses the
    /// existing benchmark + job-queue machinery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benchmark_id: Option<String>,
    /// Label → version pointers, e.g. `{"production": 3}`. A runtime fetch by `label` resolves here.
    #[serde(default)]
    pub labels: BTreeMap<String, u32>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
}

/// One immutable version of a [`Prompt`]. `version` is monotonic per prompt (1, 2, 3, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptVersion {
    #[serde(default = "crate::new_id")]
    pub id: String,
    #[serde(default)]
    pub prompt_id: String,
    #[serde(default)]
    pub version: u32,
    /// The prompt text / template.
    pub content: String,
    /// Optional structured config (model, params, variable schema). Free-form.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub config: Value,
    /// Optional change note ("commit message") describing why this version was cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}
