//! LightTrack core: the pure, I/O-free heart of the system.
//!
//! Everything here is shared by the `api`, `runner`, `mcp`, and `cli` crates:
//! the normalized [`event::LlmEvent`] model, the [`pricing::PriceBook`] and cost
//! calculation, per-project [`limits`] evaluation, and the [`score`] /benchmark types.

pub mod calibration;
pub mod collective;
pub mod customer;
pub mod dataset;
pub mod error;
pub mod event;
pub mod forecast;
pub mod job;
pub mod limits;
pub mod margin;
pub mod margin_trend;
pub mod pricing;
pub mod project;
pub mod prompt;
pub mod relay;
pub mod revenue;
pub mod rubric;
pub mod score;
pub mod trace;

pub use calibration::{agreement, Agreement, CalibrationItem};
pub use collective::{
    build_digest, merge_leaderboard, task_type_from, CollectiveDigest, CollectiveEntry,
    LeaderboardRow, ModelDigestEntry, RunStat, DEFAULT_LOW_CONFIDENCE_CASES, DEFAULT_MIN_CASES,
    DIGEST_SCHEMA_VERSION, MIN_SCHEMA_VERSION,
};
pub use customer::{BillingProduct, Customer};
pub use dataset::{Dataset, DatasetItem};
pub use error::LtError;
pub use forecast::{forecast_budget, forecast_margin, BudgetForecast, MarginForecast, Trend};
pub use job::Job;
pub use margin::{compute_margin, CostByDimension, MarginDimension, MarginRow};
pub use margin_trend::{
    compute_margin_trend, DailyKeyCost, MarginTrend, MarginTrendPoint, MarginTrendSeries,
};
pub use revenue::{RevenueEvent, RevenueKind};
pub use rubric::{Rubric, RubricDimension};
pub use event::{LlmEvent, Operation, Provider, Status, TokenUsage};
pub use limits::{
    scope_matches, LimitAction, LimitMetric, LimitRule, LimitScope, LimitStatus, LimitWindow,
};
pub use pricing::{ModelPrice, ModelPriceRow, PriceBook, PricingMode};
pub use project::{ApiKey, Project, Redaction};
pub use prompt::{Prompt, PromptVersion};
pub use relay::{
    RelayOutcome, RelayTask, RELAY_DEFAULT_MAX_ATTEMPTS, RELAY_DEFAULT_RETRY_INTERVAL_SECS,
};
pub use score::{
    judge_verdict_schema, Benchmark, BenchmarkCase, BenchmarkRun, BenchTarget, JudgeVerdict, Score,
};
pub use trace::{Trace, TraceSpan, TraceSummary, TraceTotals};

/// Convenience: a fresh UUIDv4 as a `String` (our canonical id form).
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
