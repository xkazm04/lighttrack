use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A versioned, reusable evaluation dataset. Freezing makes the dataset immutable, which fixes
/// one half of run comparability — the input. It does not fix the other half. Where the cases came
/// from outside this project (`source: import`), the models under test accumulate exposure to that
/// material as it circulates, so two runs of the same frozen dataset months apart are not
/// interchangeable: a rise can be the target or judge having *seen* the cases rather than having
/// improved. Cases sampled from this project's own traffic (`source: events:recent`) cannot be
/// exposed that way, which is the strongest reason to prefer them over an import.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Dataset {
    #[serde(default = "crate::new_id")]
    pub id: String,
    #[serde(default)]
    pub project_id: String,
    pub name: String,
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub frozen: bool,
    /// Provenance, e.g. `events:recent`, `manual`, `import`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The dataset this one was forked from (M24), when it was forked rather than created.
    ///
    /// The link is what makes `version` mean anything: without it a v2 is just another row that
    /// happens to share a name, and "is this run's corpus the same corpus as that run's" has no
    /// answer beyond string equality. Absent on every dataset created directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

fn default_version() -> u32 {
    1
}

/// One case in a dataset. `output` is a captured/candidate response; `expected` is a golden reference.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DatasetItem {
    #[serde(default = "crate::new_id")]
    pub id: String,
    #[serde(default)]
    pub dataset_id: String,
    pub input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The real event this item was sampled from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_event_id: Option<String>,
    /// Anonymization audit, e.g. `{"method":"regex+llm","redactions":3}`.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub anonymization: Value,
    /// Fingerprint of the normalised `input` (M24), stored so near-duplicate collapse is an index
    /// lookup instead of a scan of every case's text. Absent on items written before the column
    /// existed and on backends that do not compute it — which is why dedupe treats a `NULL` as "not
    /// known to be a duplicate" rather than as a match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hash: Option<String>,
}
