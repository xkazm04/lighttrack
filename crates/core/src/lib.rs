//! LightTrack core: the pure, I/O-free heart of the system.
//!
//! Everything here is shared by every other crate in the workspace — `api`, the three store
//! backends, `runner`, `mcp`, `cli`, `billing`, `responder`, `agent`, `contract` — and by the Rust
//! client SDK, which reuses [`event::LlmEvent`] as its wire type so the payload cannot drift from
//! the API. Here live the normalized event model, the [`pricing::PriceBook`] and cost calculation,
//! per-project [`limits`] evaluation, and the [`score`] / benchmark types. One file per data type;
//! `lib.rs` is the module list and the re-exports, nothing heavier.

pub mod alert;
pub mod alert_channel;
pub mod alert_sign;
pub mod alias_table;
pub mod bench_target;
pub mod calibration;
pub mod calibration_record;
pub mod collective;
pub mod customer;
pub mod dataset;
pub mod dataset_lineage;
pub mod device;
pub mod error;
pub mod event;
pub mod forecast;
pub mod forecast_gate;
pub mod job;
pub mod job_kinds;
pub mod label;
pub mod lease;
pub mod limits;
pub mod margin;
pub mod margin_policy;
pub mod margin_sim;
pub mod margin_trend;
pub mod model_id;
pub mod price_row;
pub mod pricing;
pub mod project;
pub mod prompt;
pub mod prompt_canary;
pub mod provider;
pub mod relay;
pub mod relay_verdict;
pub mod revenue;
pub mod rollup;
pub mod rubric;
pub mod schedule;
pub mod score;
pub mod trace;
pub mod unpriced;

pub use alert::{Alert, AlertKind, Delivery, Severity};
pub use alert_channel::{AlertChannel, ChannelKind};
pub use alert_sign::{
    derive_key as derive_signing_key, signature_header, verify as verify_signature,
    SIGNATURE_HEADER,
};
pub use alias_table::AliasTable;
pub use bench_target::{
    url_host, BenchTarget, PromptRef, TargetKind, INPUT_PLACEHOLDER, RESOLVED_PROMPT_VERSION,
};
pub use calibration::{agreement, Agreement, CalibrationItem};
pub use calibration_record::{CalibrationRecord, JudgeTrust, JudgeTrustVerdict};
pub use collective::{
    bucket_cost, build_digest, build_digest_counted, canon_determinism, digest_sha256, hub_url_hash, merge_leaderboard,
    normalize_hub_url, task_type_from, CollectiveDigest, CollectiveEntry, ContributionRecord,
    ContributionStatus, Coverage, LeaderboardRow, ModelAliases, ModelDigestEntry, RowRigor,
    RunStat, DEFAULT_LOW_CONFIDENCE_CASES, DEFAULT_MIN_CASES, DETERMINISM_LEVELS,
    DIGEST_SCHEMA_VERSION, MIN_SCHEMA_VERSION,
};
pub use customer::{BillingProduct, Customer};
pub use dataset::{Dataset, DatasetItem};
pub use dataset_lineage::{
    input_fingerprint, normalize_input, ImportFilter, ImportSource, ImportSpec, SamplingStrategy,
    MAX_IMPORT_N,
};
pub use device::{capability_matches, Device, DeviceEligibility, RelayAdmission};
pub use error::LtError;
pub use event::{
    FailureClass, LlmEvent, Operation, Provider, Status, TokenUsage, FAILURE_CLASS_KEY,
};
pub use forecast::{forecast_budget, forecast_margin, BudgetForecast, MarginForecast, Trend};
pub use forecast_gate::{Refusal, FLAT_BAND, MIN_OBSERVED_DAYS, MIN_SPAN_DAYS};
pub use job::{
    job_is_terminal, Job, JobCancel, JobFinish, JobKind, JOB_ERROR_PREFIX_FAILURE,
    JOB_ERROR_WORKER_LOST,
};
pub use job_kinds::{
    validate_payload, BenchRunPayload, CalibratePayload, ContributePayload, DatasetSamplePayload,
    JudgeSpec, ScoreEventsPayload, ScoreTracesPayload,
};
pub use label::{Label, LabelFilter, LabelSubject, MAX_LABEL_DIMENSIONS, MAX_LABEL_TEXT};
pub use lease::{LeaseFence, LeaseHeld};
pub use limits::{
    scope_matches, shed_ticket, CostEvidence, Escalation, LimitAction, LimitMetric, LimitRule,
    LimitScope, LimitStatus, LimitWindow, ScopeDims, Threshold, ThresholdBasis, ThresholdDimension,
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
pub use price_row::{parse_price_date, ModelPriceRow, PriceBookPosture, DEFAULT_PRICE_STALE_DAYS};
pub use pricing::{ModelPrice, PriceBook, PricingMode};
pub use project::{
    decode_scopes, default_scopes, encode_scopes, ApiKey, Project, Redaction, RedactionStamp,
    Scope, REDACTION_KEY,
};
pub use prompt::{Prompt, PromptVersion};
pub use prompt_canary::{
    CanaryPolicy, LabelChange, DEFAULT_CANARY_LABEL, DEFAULT_PRODUCTION_LABEL, MAX_LABEL_HISTORY,
    REASON_CANARY_REGRESSED, REASON_PROMOTE,
};
pub use provider::{family_of, ProviderFamily, ProviderId, UNKNOWN_PROVIDER};
pub use relay::{
    RelayOutcome, RelayStatus, RelayTask, RELAY_DEFAULT_MAX_ATTEMPTS,
    RELAY_DEFAULT_RETRY_INTERVAL_SECS, RELAY_ERROR_DEVICE_LOST, RELAY_MAX_STALE_RECLAIMS,
};
pub use relay_verdict::{RelayCancel, RelaySettle};
pub use revenue::{RevenueEvent, RevenueKind};
pub use rollup::{Dimension, RollupQuery, RollupRow, Storage, TimeKey, MAX_GROUP_BY};
pub use rubric::{
    DimensionCheck, DimensionKind, Rubric, RubricDimension, DEFAULT_RUBRIC_THRESHOLD,
};
pub use schedule::{Schedule, MIN_INTERVAL_SECS as SCHEDULE_MIN_INTERVAL_SECS};
pub use score::{
    judge_verdict_schema, Benchmark, BenchmarkCase, BenchmarkRun, JudgeVerdict, Score, ScoreDetail,
    ScoreDim, ScoreKind, MAX_DIMENSIONS, MAX_NOTES, MAX_REASONINGS_PER_DIM, MAX_REASONING_CHARS,
    RECURRENCE_KEY, REGRESSION_DATASET_KEY,
};
pub use trace::{
    normalize_trace_ref, Trace, TraceCoverage, TraceDrift, TraceShape, TraceSpan, TraceSummary,
    TraceTotals,
};
pub use unpriced::{UnpricedLedger, UnpricedRow, UNPRICED_NOTES};

/// Convenience: a fresh UUIDv4 as a `String` (our canonical id form).
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
