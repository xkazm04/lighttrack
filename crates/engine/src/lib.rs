//! The scoring/generation engine: drive `claude -p` and provider APIs (Gemini/OpenAI) to generate
//! candidate outputs and to judge them (LLM-as-judge). No HTTP-server concerns live here.
//!
//! Layout:
//! - [`prompts`]  — judge/eval/rubric prompt + schema builders (re-exported).
//! - `claude`     — the `claude -p` subprocess caller + envelope helpers.
//! - `providers`  — [`generate`] across `anthropic` / `google` / `openai` (schema-enforced + retried).
//! - `parse`      — JSON extraction + the one-shot repair re-ask around a single judge sample.
//! - `retry`      — bounded exponential backoff for transient (429/5xx/timeout) provider failures.
//! - `judge`      — [`run_judge`], [`run_rubric_judge`], [`run_text`], [`parse_judge_spec`].

mod claude;
mod judge;
mod pairwise;
mod parse;
mod pool;
mod prompts;
mod providers;
mod retry;

use lighttrack_core::JudgeVerdict;
use thiserror::Error;

pub use claude::{resolve_claude_bin, run_raw, RawOutcome};
pub use judge::{parse_judge_spec, run_judge, run_rubric_judge, run_text};
pub use pairwise::{run_pairwise, PairwiseOutcome, PairwiseVerdict, PairwiseWinner};
pub use prompts::{
    build_eval_prompt, build_judge_prompt, build_pairwise_prompt, build_rubric_prompt,
    build_rubric_schema,
};
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
    pub reasoning: String,
    pub weight: f64,
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
    pub model: String,
    pub samples: u32,
    /// Cross-sample agreement on the overall score (1.0 = identical; lower = judge disagreed).
    pub agreement: f64,
    /// How many of the `samples` judge responses were unparseable (no JSON, truncated/invalid JSON,
    /// or a dimension whose score was missing/non-numeric) and so were dropped from the means rather
    /// than scored 0.0. If *every* sample fails, the judge returns [`EngineError::Parse`] instead of
    /// emitting a phantom zero. Surfaced as an audit trail for self-consistency runs.
    pub parse_failures: u32,
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
}
