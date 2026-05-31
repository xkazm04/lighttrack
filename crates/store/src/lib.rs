//! LightTrack persistence layer.
//!
//! [`Store`] is the backend-agnostic interface used by `api` (and later `mcp`/`cli`). The local
//! implementation is [`sqlite::SqliteStore`]; a BigQuery backend slots in behind the same trait
//! when we move to the cloud (see `docs/ARCHITECTURE.md` §5).
//!
//! Methods are synchronous (SQLite is blocking). Async callers wrap them in `spawn_blocking`.

pub mod sqlite;

use serde::Serialize;
use thiserror::Error;

use lighttrack_core::LlmEvent;

pub use sqlite::SqliteStore;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A cost/usage rollup row (grouped by project + provider + model).
#[derive(Debug, Clone, Serialize)]
pub struct CostRow {
    pub project_id: String,
    pub provider: String,
    pub model: String,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
}

/// Backend-agnostic persistence interface.
pub trait Store: Send + Sync {
    /// Create tables if they don't exist.
    fn init_schema(&self) -> Result<()>;

    /// Persist one normalized event.
    fn insert_event(&self, ev: &LlmEvent) -> Result<()>;

    /// Most recent events, newest first, optionally filtered by project.
    fn list_events(&self, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>>;

    /// Cost/usage rollup grouped by project + provider + model, optionally filtered by project.
    fn cost_summary(&self, project: Option<&str>) -> Result<Vec<CostRow>>;
}
