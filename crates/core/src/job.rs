use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A unit of background work (Phase 3.6d). Enqueued by the API, executed by `lt-runner serve`,
/// so long operations (benchmark runs) never block ingestion. Cloud path swaps the table for Pub/Sub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    #[serde(default = "crate::new_id")]
    pub id: String,
    /// Job kind, e.g. `bench_run`.
    #[serde(rename = "type")]
    pub job_type: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
    /// `queued` | `running` | `done` | `failed`.
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub result: Value,
    /// When a worker last claimed it (for stale-claim recovery / heartbeat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<DateTime<Utc>>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
}

fn default_status() -> String {
    "queued".to_string()
}

fn default_max_attempts() -> u32 {
    3
}
