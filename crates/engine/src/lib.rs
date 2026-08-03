//! The scoring/generation engine: drive `claude -p` and provider APIs (Gemini/OpenAI) to generate
//! candidate outputs and to judge them (LLM-as-judge). No HTTP-server concerns live here.
//!
//! Layout:
//! - [`prompts`]  — judge/eval/rubric prompt + schema builders (re-exported).
//! - `claude`     — the `claude -p` subprocess caller + envelope helpers.
//! - `providers`  — [`generate`] across `anthropic` / `google` / `openai` (schema-enforced + retried).
//! - `parse`      — JSON extraction + the one-shot repair re-ask around a single judge sample.
//! - `fence`      — per-call nonce delimiters around untrusted content (judge-prompt injection defense).
//! - `anthropic_api` — the bare Messages API judge path (used when `ANTHROPIC_API_KEY` is set).
//! - `family`     — coarse model families, for the self-preference bias control.
//! - `retry`      — bounded exponential backoff for transient (429/5xx/timeout) provider failures.
//! - `scorers`   — deterministic (non-LLM) rubric dimensions: exact/regex/numeric/json_valid/contains.
//! - `judge`      — [`run_judge`], [`run_rubric_judge`], [`run_text`], [`parse_judge_spec`].

mod anthropic_api;
mod claude;
mod family;
mod fence;
mod judge;
mod pairwise;
mod parse;
mod pool;
mod prompts;
mod providers;
mod retry;
mod scorers;

use lighttrack_core::JudgeVerdict;
use thiserror::Error;

pub use claude::{resolve_claude_bin, run_raw, RawOutcome};
pub use judge::{parse_judge_spec, run_judge, run_rubric_judge, run_text};
pub use pairwise::{run_pairwise, PairwiseOutcome, PairwiseVerdict, PairwiseWinner};
pub use prompts::{
    build_eval_prompt, build_judge_prompt, build_pairwise_prompt, build_rubric_prompt,
    build_rubric_schema, Prompt,
};
pub use family::{model_family, same_family};
pub use providers::generate;

/// Errors from the scoring engine. Transport failures carry a typed classification (not string
/// matches) so [`retry`](crate::retry) can retry only the transient ones and the judge can tell an
/// empty completion apart from output that failed to parse.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("failed to spawn '{bin}': {source}")]
    Spawn {
        bin: String,
        source: std::io::Error,
    },
    #[error("claude exited with status {code}: {stderr}")]
    NonZero { code: i32, stderr: String },
    /// HTTP 429 — retryable.
    #[error("{who} rate-limited (HTTP 429)")]
    RateLimited { who: String },
    /// HTTP 5xx — retryable.
    #[error("{who} server error (HTTP {status})")]
    ServerError { who: String, status: u16 },
    /// Connect/read timeout — retryable.
    #[error("{who} request timed out")]
    Timeout { who: String },
    /// HTTP 4xx other than 429/401/403 — often a rejected JSON schema; triggers the schema-less
    /// prose fallback in [`generate`](crate::generate).
    #[error("{who} rejected the request (HTTP {status}): {body}")]
    BadRequest { who: String, status: u16, body: String },
    /// HTTP 401/403 — a credentials problem; not retryable.
    #[error("{who} authentication failed (HTTP {status})")]
    Auth { who: String, status: u16 },
    /// The provider returned no completion text (distinct from unparseable output).
    #[error("{who} returned an empty completion")]
    EmptyCompletion { who: String },
    /// A non-transient transport error (DNS, TLS, malformed response, …).
    #[error("{who} request failed: {detail}")]
    Http { who: String, detail: String },
    #[error("could not parse judge output: {0}")]
    Parse(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// How reproducible a judge call actually was — stamped on every outcome so a score's determinism
/// is a recorded fact rather than an assumption about the provider.
///
/// A verdict is a measurement, so agreement between self-consistency samples should signal a
/// genuinely ambiguous case, not sampling noise. Whether that holds depends on which knobs the
/// provider exposes, so we record what we got instead of claiming what we wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Determinism {
    /// **Every** sampling control the provider exposes was pinned, including a fixed seed —
    /// re-running the eval reproduces the verdict by contract. OpenAI and Gemini (`temperature: 0`
    /// + `JUDGE_SEED`).
    Exact,
    /// Reproducibility is convention, not contract: the path exposed no seed (the Anthropic
    /// Messages API has none — `temperature: 0` is its whole sampling surface), exposed no sampling
    /// knobs at all (the `claude -p` CLI), or rejected the ones we asked for and we retried without
    /// them. Agreement on such a run partly measures sampling noise.
    BestEffort,
}

impl Determinism {
    pub fn as_str(self) -> &'static str {
        match self {
            Determinism::Exact => "exact",
            Determinism::BestEffort => "best-effort",
        }
    }

    /// The weaker of two stamps — a run is only as deterministic as its least deterministic call.
    pub fn weakest(self, other: Determinism) -> Determinism {
        if self == Determinism::Exact && other == Determinism::Exact {
            Determinism::Exact
        } else {
            Determinism::BestEffort
        }
    }
}

impl std::fmt::Display for Determinism {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How to invoke the engine (provider+model are passed per call; this holds the Claude CLI config).
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub claude_bin: String,
    pub model: String,
    /// Pass `--bare` to skip auto-loading hooks/skills/MCP/CLAUDE.md. Avoids re-caching ~40k tokens
    /// per call, but bypasses subscription OAuth, so it requires `ANTHROPIC_API_KEY` in the env.
    pub bare: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            claude_bin: "claude".to_string(),
            model: "haiku".to_string(),
            bare: false,
        }
    }
}

/// The result of one judge call.
#[derive(Debug, Clone)]
pub struct JudgeOutcome {
    pub verdict: JudgeVerdict,
    pub cost_usd: Option<f64>,
    pub model: String,
    pub session_id: Option<String>,
    pub latency_ms: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Content under evaluation imitated a prompt boundary and was neutralized before the judge saw
    /// it (see [`fence`]). Not a verdict on the content — a signal that this case tried to talk to
    /// the judge, worth surfacing next to the score it produced.
    pub injection_suspected: bool,
    /// Whether this verdict is reproducible by contract or only by convention.
    pub determinism: Determinism,
}

/// The result of a free-form text call (e.g. LLM-based anonymization / healing).
#[derive(Debug, Clone)]
pub struct TextOutcome {
    pub text: String,
    pub cost_usd: Option<f64>,
    pub model: String,
    pub latency_ms: Option<u64>,
}

/// One dimension's aggregated score within a rubric judgement.
#[derive(Debug, Clone)]
pub struct DimScore {
    pub key: String,
    pub score: f64,
    /// The judge's reasoning for this dimension, **one entry per sample that parsed**, in sample
    /// order. Every sample is kept: a k-sample run bills k sets of reasoning tokens, and keeping
    /// only the first made samples 2..k pure waste.
    pub reasonings: Vec<String>,
    pub weight: f64,
    /// The rubric's gating floor for this dimension, when it has one.
    pub floor: Option<f64>,
    /// `score` fell below `floor` — the reason a passing overall can still fail.
    pub floor_hit: bool,
}

impl DimScore {
    /// The representative (first-sample) reasoning, for callers that want one line.
    pub fn reasoning(&self) -> &str {
        self.reasonings.first().map(String::as_str).unwrap_or("")
    }
}

/// The result of judging one case against a rubric (possibly averaged over k samples).
#[derive(Debug, Clone)]
pub struct RubricOutcome {
    pub dimensions: Vec<DimScore>,
    pub overall: f64,
    pub pass: bool,
    pub cost_usd: Option<f64>,
    pub latency_ms: Option<u64>,
    pub tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// The judge model, or `"deterministic"` when the rubric needed no model call at all.
    pub model: String,
    /// Judge samples requested (`0` for an all-deterministic rubric — nothing was sampled).
    pub samples: u32,
    /// How many of the requested `samples` produced a usable verdict (`samples - parse_failures`).
    pub samples_parsed: u32,
    /// Cross-sample agreement on the overall score (1.0 = identical; lower = judge disagreed).
    /// Measured over the **LLM dimensions only**: a deterministic dimension is scored once and is
    /// exactly reproducible, so including it would inflate a number that is meant to describe the
    /// judge model's stability. An all-deterministic rubric reports `1.0` over zero samples.
    pub agreement: f64,
    /// How many of the `samples` judge responses were unparseable (no JSON, truncated/invalid JSON,
    /// or a dimension whose score was missing/non-numeric) and so were dropped from the means rather
    /// than scored 0.0. If *every* sample fails, the judge returns [`EngineError::Parse`] instead of
    /// emitting a phantom zero. Surfaced as an audit trail for self-consistency runs.
    pub parse_failures: u32,
    /// The judged content (input/reference/output, or model text echoed on a repair re-ask) imitated
    /// a prompt boundary and was neutralized. See [`JudgeOutcome::injection_suspected`].
    pub injection_suspected: bool,
    /// The weakest determinism stamp across this case's samples (including repair re-asks).
    pub determinism: Determinism,
}

/// The result of generating one candidate output from a target.
#[derive(Debug, Clone)]
pub struct GenOutcome {
    pub output: String,
    pub cost_usd: Option<f64>,
    pub model: String,
    pub latency_ms: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// How reproducible this call was — see [`Determinism`]. Candidate *generation* is not a
    /// measurement and is not asked to be deterministic, so it reports `BestEffort`.
    pub determinism: Determinism,
}
