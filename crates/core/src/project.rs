use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How prompt/output payloads are persisted for a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Redaction {
    /// Store payloads as sent.
    None,
    /// Store only a hash of payloads (presence/diff without content).
    Hash,
    /// Never persist payloads.
    Drop,
}

impl Default for Redaction {
    fn default() -> Self {
        Redaction::None
    }
}

/// A monitored application / tenant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub redaction: Redaction,
    /// Consent to include this project's benchmark runs in a collective-network digest. Default
    /// **off**: contributing a project's eval results to a shared hub is an act, not an inheritance —
    /// `lt collective contribute` must never ship a project nobody opted in.
    #[serde(default)]
    pub collective_opt_in: bool,
    pub created_at: DateTime<Utc>,
}

fn default_true() -> bool {
    true
}

/// An ingest API key. Only `key_hash` is persisted; the raw secret is shown once at creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    pub project_id: String,
    pub name: String,
    /// Non-secret, human-recognizable prefix, e.g. `lt_ab12cd`.
    pub prefix: String,
    /// Salted hash of the full secret (hashing lives in the `api` crate).
    pub key_hash: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub revoked: bool,
}
