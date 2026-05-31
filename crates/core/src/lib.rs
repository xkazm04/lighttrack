//! LightTrack core: the pure, I/O-free heart of the system.
//!
//! Everything here is shared by the `api`, `runner`, `mcp`, and `cli` crates:
//! the normalized [`event::LlmEvent`] model, the [`pricing::PriceBook`] and cost
//! calculation, per-project [`limits`] evaluation, and the [`score`] /benchmark types.

pub mod calibration;
pub mod dataset;
pub mod error;
pub mod event;
pub mod job;
pub mod limits;
pub mod pricing;
pub mod project;
pub mod rubric;
pub mod score;

pub use calibration::{agreement, Agreement, CalibrationItem};
pub use dataset::{Dataset, DatasetItem};
pub use error::LtError;
pub use job::Job;
pub use rubric::{Rubric, RubricDimension};
pub use event::{LlmEvent, Operation, Provider, Status, TokenUsage};
pub use limits::{LimitAction, LimitMetric, LimitRule, LimitStatus, LimitWindow};
pub use pricing::{ModelPrice, ModelPriceRow, PriceBook};
pub use project::{ApiKey, Project, Redaction};
pub use score::{
    judge_verdict_schema, Benchmark, BenchmarkCase, BenchmarkRun, BenchTarget, JudgeVerdict, Score,
};

/// Convenience: a fresh UUIDv4 as a `String` (our canonical id form).
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
