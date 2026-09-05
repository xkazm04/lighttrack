//! Collective Model Intelligence — privacy-safe aggregation of benchmark results into a shareable
//! model leaderboard.
//!
//! The network effect: every LightTrack instance runs benchmarks on *its own real tasks*. This module
//! turns those runs into a **privacy-safe digest** — aggregate `(provider, model, task_type)` →
//! quality / cost / latency, carrying **no raw text, no project ids, no customer data**, only public
//! model identities and aggregate numbers. Instances opt in to contribute their digest to a shared hub
//! (another LightTrack acting as the public leaderboard); the hub merges contributions so every
//! operator sees real-world model performance instead of vendor benchmarks.
//!
//! Three privacy guarantees are enforced here, in pure code, so they hold for every backend:
//!   1. **Aggregate-only inputs.** A digest is built from benchmark *run scorecards* ([`RunStat`]),
//!      which already carry no prompt/response text — we never touch `events`.
//!   2. **k-anonymity.** A `(provider, model, task_type)` bucket is published only when it aggregates
//!      at least `min_cases` cases, so a rare/unique task can't be fingerprinted to one operator.
//!   3. **Bucketed cost.** A per-case cost is an unbounded continuous value and therefore a
//!      fingerprint; it is coarsened by [`bucket_cost`] before it leaves the instance.
//!
//! The coarse `task_type` is always one of a fixed vocabulary ([`task_type_from`]); a custom benchmark
//! name is classified into a bucket, never published verbatim.
//!
//! **Merge honesty (digest schema v2).** A point estimate is misleading when a 5-case bucket ranks
//! next to a 50k-case one. Each v2 entry carries a `quality_variance` (population variance across the
//! contributing runs' case-weighted mean scores), so the merge can attach an *approximate* 95% CI to
//! each leaderboard mean and flag thin rows as low-confidence. See the `merge` submodule for the
//! estimator and its documented approximations.

mod aliases;
mod classify;
mod contribution;
mod merge;
mod privacy;
pub mod rigor;
mod spread;
mod types;

/// Current digest wire-format version. Bump when [`ModelDigestEntry`] changes shape. v2 added
/// `quality_variance` (and, in the same release, judge/rubric tags); v3 added the **rigor** block
/// (`determinism`, `frozen_dataset`, `significance_tested`).
pub const DIGEST_SCHEMA_VERSION: u32 = 3;

/// Oldest digest wire-format version a hub still accepts. v1 digests carry no variance and v1/v2 no
/// rigor; they merge with `quality_variance = None` (CI unknown) and `Coverage::Unknown` rigor rather
/// than being orphaned by a version bump — every added field is additive and serde-defaulted.
pub const MIN_SCHEMA_VERSION: u32 = 1;

/// Default k-anonymity floor: a bucket needs at least this many cases to be published.
pub const DEFAULT_MIN_CASES: u32 = 5;

/// Default display floor: a merged row aggregating fewer cases than this is flagged `low_confidence`
/// (shown, not hidden) so a thin sample can't masquerade as an authoritative ranking.
pub const DEFAULT_LOW_CONFIDENCE_CASES: u32 = 30;

/// Contributor id used when the operator sets no stable id.
pub const ANON_CONTRIBUTOR: &str = "anonymous";

/// The fixed task-type vocabulary a benchmark is classified into. Publishing only these labels (never
/// a raw benchmark name) keeps the digest from leaking project-specific naming.
pub const TASK_TYPES: &[&str] = &[
    "summarization",
    "qa",
    "extraction",
    "classification",
    "translation",
    "coding",
    "reasoning",
    "rag",
    "generation",
    "general",
];

pub use aliases::ModelAliases;
pub use classify::task_type_from;
pub use contribution::{
    digest_sha256, hub_url_hash, normalize_hub_url, ContributionRecord, ContributionStatus,
};
pub use merge::{build_digest, build_digest_counted, merge_leaderboard, MAX_SOURCE_WEIGHT_SHARE};
pub use privacy::bucket_cost;
pub use rigor::{canon_determinism, Coverage, RowRigor, DETERMINISM_LEVELS};
pub use types::{CollectiveDigest, CollectiveEntry, LeaderboardRow, ModelDigestEntry, RunStat};
