//! The **one** headless-Claude invocation seam.
//!
//! Every `claude -p` spawn in this workspace goes through [`run`]: the judge/generation engine, the
//! alert responder's read-only investigation and auto-fix runs, and the device agent's relay
//! actions. Before this module each of those built its own `Command`, which meant three different
//! answers to the questions that actually decide what a paid run may do — which tools it may call,
//! which directory it sees, whether the billing key is in its environment, how long it may hang.
//! Those answers now live in exactly one place ([`posture`]), and a contradiction between them is an
//! error *before* a child is spawned rather than a surprise afterwards.
//!
//! Layout:
//! - `posture`   — [`Mode`] → argv shape, tool allowlist, cwd and auth policy (the enforcement).
//! - `run`       — spawn, prompt over **stdin**, bounded reaper, envelope parse.
//! - `envelope`  — reading the `--output-format json` envelope (text / usage / model / effort).
//! - `probe`     — is the CLI installed and plausibly authed, *before* we claim paid work.
//! - `resolve`   — the single `claude` executable resolver (Windows npm/native shims).

mod envelope;
mod posture;
mod probe;
mod resolve;
mod run;

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

pub use posture::{Mode, READONLY_BASE_TOOLS};
pub use probe::{probe, Probe};
pub use resolve::resolve_claude_bin;
pub use run::run;

pub(crate) use envelope::{completion_text, model_of, token_counts};

/// Wall-clock ceiling for a single `claude -p` subprocess when the caller names none.
///
/// `Command::output()` blocks until the child exits with no ceiling, so a hung `claude` (network
/// stall, stuck MCP child, an auth prompt waiting on a tty) pinned a worker thread indefinitely —
/// and with `--jobs N` a few such hangs starve the pool and the run never completes and never
/// fails. Generous, because a high-effort judge run is legitimately long; the point is to reap a
/// *hung* child, not a slow one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Where the Claude CLI lives. Resolved once by [`resolve_claude_bin`].
#[derive(Debug, Clone)]
pub struct ClaudeBin {
    pub bin: String,
}

impl ClaudeBin {
    pub fn new(bin: impl Into<String>) -> Self {
        ClaudeBin { bin: bin.into() }
    }
}

/// One headless Claude call, fully described. Build it with [`Invocation::generate`],
/// [`Invocation::readonly_scan`] or [`Invocation::edit`] and refine with the `with_*` setters —
/// the constructor picks the [`Mode`], and the mode is what the posture rules enforce.
#[derive(Debug, Clone)]
pub struct Invocation<'a> {
    pub prompt: &'a str,
    pub model: &'a str,
    pub mode: Mode,
    /// Working directory. Forbidden for [`Mode::Generate`] (which always runs in a neutral
    /// directory), required for [`Mode::Edit`].
    pub cwd: Option<PathBuf>,
    /// Extra tools beyond the mode's base allowlist. Must be empty for [`Mode::Generate`].
    pub allowed_tools: Vec<String>,
    /// `--permission-mode`. Forbidden for [`Mode::Generate`], required for [`Mode::Edit`].
    pub permission_mode: Option<&'a str>,
    pub system: Option<&'a str>,
    pub schema: Option<&'a str>,
    /// `--effort`. `None` derives it from a trailing `@<effort>` on `model`.
    pub effort: Option<&'a str>,
    pub budget_usd: Option<f64>,
    pub timeout: Duration,
    /// `--bare`: skip auto-loaded hooks/skills/MCP/CLAUDE.md. Bypasses subscription OAuth, so it
    /// *requires* `ANTHROPIC_API_KEY`; `false` is a seat run and the key is stripped from the child.
    pub bare: bool,
}

impl<'a> Invocation<'a> {
    fn new(prompt: &'a str, model: &'a str, mode: Mode) -> Self {
        Invocation {
            prompt,
            model,
            mode,
            cwd: None,
            allowed_tools: Vec::new(),
            permission_mode: None,
            system: None,
            schema: None,
            effort: None,
            budget_usd: None,
            timeout: DEFAULT_TIMEOUT,
            bare: false,
        }
    }

    /// A toolless completion in a neutral directory — judging, candidate generation, relay actions.
    pub fn generate(prompt: &'a str, model: &'a str) -> Self {
        Self::new(prompt, model, Mode::Generate)
    }

    /// A read-only agentic run over a repository (the responder's investigation).
    pub fn readonly_scan(prompt: &'a str, model: &'a str) -> Self {
        Self::new(prompt, model, Mode::ReadonlyScan)
    }

    /// An edit-capable agentic run — needs an explicit workspace *and* permission mode.
    pub fn edit(prompt: &'a str, model: &'a str) -> Self {
        Self::new(prompt, model, Mode::Edit)
    }

    /// Build for an already-decided [`Mode`] (the relay action library names one in `action.toml`).
    pub fn with_mode(prompt: &'a str, model: &'a str, mode: Mode) -> Self {
        Self::new(prompt, model, mode)
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_allowed_tools<S: Into<String>>(
        mut self,
        tools: impl IntoIterator<Item = S>,
    ) -> Self {
        self.allowed_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_permission_mode(mut self, mode: Option<&'a str>) -> Self {
        self.permission_mode = mode;
        self
    }

    pub fn with_system(mut self, system: Option<&'a str>) -> Self {
        self.system = system;
        self
    }

    pub fn with_schema(mut self, schema: Option<&'a str>) -> Self {
        self.schema = schema;
        self
    }

    pub fn with_effort(mut self, effort: Option<&'a str>) -> Self {
        self.effort = effort;
        self
    }

    pub fn with_budget_usd(mut self, budget: Option<f64>) -> Self {
        self.budget_usd = budget;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_bare(mut self, bare: bool) -> Self {
        self.bare = bare;
        self
    }
}

/// The result of one headless Claude call — the whole envelope plus the fields every caller reads.
#[derive(Debug, Clone)]
pub struct RawOutcome {
    /// The completion: the structured answer (serialized) when `--json-schema` was used, else the
    /// free-text `result`.
    pub text: String,
    /// The structured answer as JSON, when the call asked for one and got one.
    pub json: Option<Value>,
    /// The model the envelope reports, falling back to the requested one.
    pub model: String,
    pub cost_usd: Option<f64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub latency_ms: Option<u64>,
    /// The parsed envelope, for callers that need a field this struct doesn't name.
    pub raw: Value,
    /// The CLI exited 0.
    pub exit_ok: bool,
    /// The envelope's `is_error` — a *controlled* failure (budget cap, refusal) that still produced
    /// a well-formed envelope, as opposed to a crash.
    pub is_error: bool,
    /// The envelope's `subtype`, when it carries one (`error_max_budget`, …).
    pub subtype: String,
    /// The child's stderr, kept only when the run did not come back clean — it is the only place a
    /// controlled failure explains itself.
    pub stderr: String,
}

impl RawOutcome {
    /// The run both exited cleanly and reported no error.
    pub fn ok(&self) -> bool {
        self.exit_ok && !self.is_error
    }
}
