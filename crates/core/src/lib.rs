//! LightTrack core: the pure, I/O-free heart of the system.
//!
//! Everything here is shared by the `api`, `runner`, `mcp`, and `cli` crates:
//! the normalized [`event::LlmEvent`] model, the [`pricing::PriceBook`] and cost
//! calculation, per-project [`limits`] evaluation, and the [`score`] /benchmark types.

pub mod alias_table;
pub mod calibration;
pub mod collective;
pub mod customer;
pub mod dataset;
pub mod error;
pub mod event;
pub mod forecast;
pub mod job;
pub mod job_kinds;
pub mod lease;
pub mod limits;
pub mod margin;
pub mod margin_policy;
pub mod margin_sim;
pub mod margin_trend;
pub mod model_id;
pub mod pricing;
pub mod project;
pub mod prompt;
pub mod provider;
pub mod relay;
pub mod relay_verdict;
pub mod revenue;
pub mod rollup;
pub mod rubric;
pub mod schedule;
pub mod score;
pub mod trace;

pub use alias_table::AliasTable;
pub use calibration::{agreement, Agreement, CalibrationItem};
pub use collective::{
    bucket_cost, build_digest, canon_determinism, merge_leaderboard, task_type_from,
    CollectiveDigest, CollectiveEntry, Coverage, LeaderboardRow, ModelAliases, ModelDigestEntry,
    RowRigor, RunStat, DEFAULT_LOW_CONFIDENCE_CASES, DEFAULT_MIN_CASES, DETERMINISM_LEVELS,
    DIGEST_SCHEMA_VERSION, MIN_SCHEMA_VERSION,
};
pub use customer::{BillingProduct, Customer};
pub use dataset::{Dataset, DatasetItem};
pub use error::LtError;
pub use event::{LlmEvent, Operation, Provider, Status, TokenUsage};
pub use forecast::{forecast_budget, forecast_margin, BudgetForecast, MarginForecast, Trend};
pub use job::{
    job_is_terminal, Job, JobCancel, JobFinish, JobKind, JOB_ERROR_PREFIX_FAILURE,
    JOB_ERROR_WORKER_LOST,
};
pub use job_kinds::{
    validate_payload, BenchRunPayload, CalibratePayload, DatasetSamplePayload, JudgeSpec,
    ScoreEventsPayload, ScoreTracesPayload,
};
pub use lease::{LeaseFence, LeaseHeld};
pub use limits::{
    scope_matches, CostEvidence, Escalation, LimitAction, LimitMetric, LimitRule, LimitScope,
    LimitStatus, LimitWindow, ScopeDims, Threshold, ThresholdBasis, ThresholdDimension,
    ThresholdKind, DEFAULT_THROTTLE_START,
};
pub use margin::{compute_margin, CostByDimension, MarginDimension, MarginRow};
pub use margin_policy::{
    evaluate_policies, recognized_revenue, MarginPolicy, PolicyAction, PolicyTrigger, RuleChange,
    POLICY_ORIGIN_PREFIX,
};
pub use margin_sim::{compute_margin_simulation, SimAssumptions, SimRow, TokensByDimension};
pub use margin_trend::{
    compute_margin_trend, DailyKeyCost, MarginTrend, MarginTrendPoint, MarginTrendSeries,
};
pub use model_id::{canonicalize, canonicalize_with, judge_family, ModelId};
pub use pricing::{ModelPrice, ModelPriceRow, PriceBook, PricingMode};
pub use project::{
    decode_scopes, default_scopes, encode_scopes, ApiKey, Project, Redaction, RedactionStamp,
    Scope, REDACTION_KEY,
};
pub use prompt::{Prompt, PromptVersion};
pub use provider::{family_of, ProviderFamily, ProviderId, UNKNOWN_PROVIDER};
pub use relay::{
    RelayOutcome, RelayStatus, RelayTask, RELAY_DEFAULT_MAX_ATTEMPTS,
    RELAY_DEFAULT_RETRY_INTERVAL_SECS, RELAY_ERROR_DEVICE_LOST, RELAY_MAX_STALE_RECLAIMS,
};
pub use relay_verdict::{RelayCancel, RelaySettle};
pub use revenue::{RevenueEvent, RevenueKind};
pub use rollup::{Dimension, RollupQuery, RollupRow, Storage, TimeKey, MAX_GROUP_BY};
pub use rubric::{DimensionCheck, DimensionKind, Rubric, RubricDimension};
pub use schedule::{Schedule, MIN_INTERVAL_SECS as SCHEDULE_MIN_INTERVAL_SECS};
pub use score::{
    judge_verdict_schema, BenchTarget, Benchmark, BenchmarkCase, BenchmarkRun, JudgeVerdict, Score,
    ScoreDetail, ScoreDim, ScoreKind, MAX_DIMENSIONS, MAX_NOTES, MAX_REASONINGS_PER_DIM,
    MAX_REASONING_CHARS, RECURRENCE_KEY,
};
pub use trace::{
    normalize_trace_ref, Trace, TraceCoverage, TraceDrift, TraceShape, TraceSpan, TraceSummary,
    TraceTotals,
};

/// Convenience: a fresh UUIDv4 as a `String` (our canonical id form).
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
