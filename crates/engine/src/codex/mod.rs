//! The Codex CLI (`codex exec`) as a generation path — GPT models on a ChatGPT seat.
//!
//! The counterpart of the `claude -p` path: a subscription-metered CLI rather than a metered API, so
//! a matrix can walk a GPT model's whole effort ladder without spending API money. Provider id
//! `codex`, matched on the id like OpenRouter's, because a CLI is not a lab — the model name
//! (`gpt-5.5`) is what the self-preference control reads, and that says OpenAI.
//!
//! **Isolation is the posture.** A bare `codex exec` loads the operator's `config.toml` (their
//! default model and effort), their execpolicy rules, their `AGENTS.md` and their saved sessions —
//! any of which would silently join the prompt and make the same case mean different things on two
//! machines. So every call is `--ignore-user-config --ignore-rules --ephemeral`, runs in a neutral
//! directory with no `AGENTS.md`, and is sandboxed `read-only`: a generation has no business
//! touching a disk. `OPENAI_API_KEY` / `CODEX_API_KEY` are stripped from the child for the same
//! reason `ANTHROPIC_API_KEY` is on the Claude path — a seat run must not quietly become metered.
//!
//! **What it cannot give, stated:** no dollar cost (a seat call has none to report), so these targets
//! are unpriced and sit off the cost–quality frontier by name; and no sampling knobs, so `best-effort`
//! determinism. Every call also carries a ~13k-token input floor — the Codex harness's own
//! instructions — which is visible in `input_tokens` and is the same for every rung of a ladder.

mod events;
mod resolve;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Once, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use lighttrack_core::Effort;

pub use resolve::resolve_codex_bin;

use crate::invocation::spawn_bounded;
use crate::{Determinism, EngineError, GenOutcome, Result, SchemaEnforcement};

/// The provider id that routes here.
pub(crate) const PROVIDER_ID: &str = "codex";

/// The label errors and timeouts carry.
const WHO: &str = "codex exec";

/// Wall clock for one call, and for one that asked to think hard. A reaper for a *hung* child, not a
/// budget for a slow one — an `xhigh` turn on a hard case legitimately runs for many minutes.
const TIMEOUT: Duration = Duration::from_secs(600);
const TIMEOUT_HIGH_EFFORT: Duration = Duration::from_secs(1500);

/// Longest system prompt passed through argv. It travels as a `-c developer_instructions=` value,
/// and Windows caps a whole command line near 32k characters; a longer prompt is refused by name
/// rather than truncated by the OS into a different prompt.
const MAX_INSTRUCTIONS_CHARS: usize = 16_000;

fn bin() -> &'static str {
    static BIN: OnceLock<String> = OnceLock::new();
    BIN.get_or_init(resolve_codex_bin)
}

/// The argv for one isolated, tool-less turn. The prompt is not here: it travels over stdin (`-`).
fn argv(
    model: &str,
    system_prompt: Option<&str>,
    effort: Option<Effort>,
    schema_file: Option<&Path>,
) -> Result<Vec<String>> {
    let mut a: Vec<String> = [
        "exec",
        "--json",
        "--skip-git-repo-check",
        "--ephemeral",
        "--ignore-user-config",
        "--ignore-rules",
        "--sandbox",
        "read-only",
        "--model",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push(model.to_string());
    if let Some(level) = effort {
        // Quoted, so it is parsed as a TOML string rather than falling back to a bare literal.
        a.push("-c".into());
        a.push(format!("model_reasoning_effort=\"{}\"", level.as_str()));
    }
    if let Some(sys) = system_prompt {
        if sys.chars().count() > MAX_INSTRUCTIONS_CHARS {
            return Err(EngineError::Other(format!(
                "the codex adapter passes a system prompt on the command line and caps it at \
                 {MAX_INSTRUCTIONS_CHARS} characters; this one is {} — a longer one would be \
                 truncated by the OS into a different prompt",
                sys.chars().count()
            )));
        }
        // A JSON string literal is a valid TOML basic string, so quotes and newlines survive intact.
        a.push("-c".into());
        a.push(format!(
            "developer_instructions={}",
            serde_json::to_string(sys)?
        ));
    }
    if let Some(path) = schema_file {
        a.push("--output-schema".into());
        a.push(path.display().to_string());
    }
    a.push("-".into());
    Ok(a)
}

/// A directory with no `AGENTS.md` and no repository, apart from the Claude path's own.
fn neutral_cwd() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("lighttrack-codex-neutral");
    std::fs::create_dir_all(&dir)
        .map_err(|e| EngineError::Other(format!("creating '{}': {e}", dir.display())))?;
    Ok(dir)
}

/// `--output-schema` takes a file. One per call, named uniquely, removed after the call.
fn write_schema(schema: &Value) -> Result<PathBuf> {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = neutral_cwd()?.join(format!(
        "schema-{}-{}.json",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, serde_json::to_vec(schema)?)
        .map_err(|e| EngineError::Other(format!("writing '{}': {e}", path.display())))?;
    Ok(path)
}

static AUTH_LOG: Once = Once::new();

fn strip_api_keys(cmd: &mut Command) {
    let mut present = Vec::new();
    for key in ["OPENAI_API_KEY", "CODEX_API_KEY"] {
        if std::env::var_os(key).is_some_and(|v| !v.is_empty()) {
            present.push(key);
        }
        cmd.env_remove(key);
    }
    AUTH_LOG.call_once(|| {
        let note = if present.is_empty() {
            String::new()
        } else {
            format!(" ({} stripped from the child)", present.join(", "))
        };
        eprintln!("[engine] codex auth: ChatGPT seat login; API keys unused{note}");
    });
}

/// Turn a failure the CLI reported into a typed error, so the retry policy retries only what is
/// worth retrying.
fn classify(status: Option<u16>, message: String) -> EngineError {
    let who = WHO.to_string();
    let lower = message.to_ascii_lowercase();
    match status {
        Some(429) => EngineError::RateLimited {
            who,
            retry_after: None,
        },
        Some(s @ (401 | 403)) => EngineError::Auth { who, status: s },
        Some(s) if s >= 500 => EngineError::ServerError { who, status: s },
        Some(s) => EngineError::BadRequest {
            who,
            status: s,
            body: message,
        },
        None if lower.contains("usage limit") || lower.contains("rate limit") => {
            EngineError::RateLimited {
                who,
                retry_after: None,
            }
        }
        None => EngineError::Other(format!("{WHO}: {message}")),
    }
}

/// Generate one candidate through `codex exec`.
pub(crate) fn generate(
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
    effort: Option<Effort>,
) -> Result<GenOutcome> {
    let schema_file = schema.map(write_schema).transpose()?;
    let args = argv(model, system_prompt, effort, schema_file.as_deref())?;
    let mut cmd = Command::new(bin());
    cmd.args(&args)
        .current_dir(neutral_cwd()?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    strip_api_keys(&mut cmd);
    let timeout = match effort {
        Some(Effort::XHigh) | Some(Effort::Max) => TIMEOUT_HIGH_EFFORT,
        _ => TIMEOUT,
    };

    let started = Instant::now();
    let spawned = spawn_bounded(cmd, input, timeout, bin(), WHO);
    let latency_ms = Some(started.elapsed().as_millis() as u64);
    if let Some(f) = &schema_file {
        let _ = std::fs::remove_file(f);
    }
    let (status, stdout, stderr) = spawned?;

    let turn = events::read(&String::from_utf8_lossy(&stdout));
    // The CLI's own report of what went wrong outranks its exit code: it is the only place the API's
    // message ("requires a newer version of Codex") survives.
    if let Some(message) = turn.error {
        return Err(classify(turn.status, message));
    }
    if !status.success() {
        return Err(EngineError::NonZero {
            code: status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
        });
    }
    let output = turn.text.unwrap_or_default();
    if output.trim().is_empty() {
        return Err(EngineError::EmptyCompletion { who: WHO.into() });
    }
    let usage = turn.usage.unwrap_or_default();
    Ok(GenOutcome {
        output,
        // A seat call reports no dollars; the caller's price book decides, and will find none.
        cost_usd: None,
        // The event stream never names the model, so the requested one is the honest identity.
        model: model.to_string(),
        latency_ms,
        input_tokens: usage.input,
        output_tokens: usage.output,
        reasoning_tokens: usage.reasoning,
        // No temperature, no seed: nothing to pin.
        determinism: Determinism::BestEffort,
        schema: if schema.is_some() {
            SchemaEnforcement::Enforced
        } else {
            SchemaEnforcement::NotRequested
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_pair(a: &[String], flag: &str, value: &str) -> bool {
        a.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    /// **The isolation posture, asserted where it is written.** Every one of these keeps something
    /// of the operator's out of the measurement: their config (default model and effort), their
    /// rules, their saved sessions, write access to a disk.
    #[test]
    fn every_call_is_isolated_read_only_and_reads_its_prompt_from_stdin() {
        let a = argv("gpt-5.5", None, None, None).unwrap();
        assert_eq!(a[0], "exec");
        for flag in [
            "--json",
            "--skip-git-repo-check",
            "--ephemeral",
            "--ignore-user-config",
            "--ignore-rules",
        ] {
            assert!(a.iter().any(|x| x == flag), "missing {flag}: {a:?}");
        }
        assert!(has_pair(&a, "--sandbox", "read-only"));
        assert!(has_pair(&a, "--model", "gpt-5.5"));
        assert_eq!(a.last().map(String::as_str), Some("-"), "prompt over stdin");
        // No effort asked for: the key is absent, so the model's default applies and is not reported
        // as a level.
        assert!(!a.iter().any(|x| x.starts_with("model_reasoning_effort")));
    }

    #[test]
    fn every_level_travels_as_a_quoted_toml_string() {
        for level in Effort::ALL {
            let a = argv("gpt-5.5", None, Some(level), None).unwrap();
            assert!(
                has_pair(
                    &a,
                    "-c",
                    &format!("model_reasoning_effort=\"{}\"", level.as_str())
                ),
                "{a:?}"
            );
        }
    }

    /// A system prompt with quotes and newlines arrives intact: the value is a JSON string literal,
    /// which TOML reads as the same string.
    #[test]
    fn a_system_prompt_survives_quotes_and_newlines() {
        let sys = "Answer \"tersely\".\nNo preamble.\tEver.";
        let a = argv("gpt-5.5", Some(sys), None, None).unwrap();
        let v = a
            .iter()
            .find_map(|x| x.strip_prefix("developer_instructions="))
            .expect("instructions passed");
        assert_eq!(serde_json::from_str::<String>(v).unwrap(), sys);
    }

    #[test]
    fn an_oversized_system_prompt_is_refused_by_name() {
        let huge = "x".repeat(MAX_INSTRUCTIONS_CHARS + 1);
        let err = argv("gpt-5.5", Some(&huge), None, None).unwrap_err();
        assert!(err.to_string().contains("caps it at"), "{err}");
        assert!(argv(
            "gpt-5.5",
            Some(&"x".repeat(MAX_INSTRUCTIONS_CHARS)),
            None,
            None
        )
        .is_ok());
    }

    #[test]
    fn a_schema_is_passed_by_file_before_the_stdin_marker() {
        let a = argv("gpt-5.5", None, None, Some(Path::new("s.json"))).unwrap();
        assert!(has_pair(&a, "--output-schema", "s.json"));
        assert_eq!(a.last().map(String::as_str), Some("-"));
    }

    /// Only what is worth retrying is retried: a rate limit and a server error are; a refused model
    /// and a bad login are not.
    #[test]
    fn failures_classify_by_status_and_by_what_a_seat_limit_says() {
        assert!(matches!(
            classify(Some(429), "x".into()),
            EngineError::RateLimited { .. }
        ));
        assert!(matches!(
            classify(Some(503), "x".into()),
            EngineError::ServerError { .. }
        ));
        assert!(matches!(
            classify(Some(401), "x".into()),
            EngineError::Auth { .. }
        ));
        match classify(Some(400), "requires a newer version of Codex".into()) {
            EngineError::BadRequest { body, .. } => assert!(body.contains("newer version")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            classify(None, "You've hit your usage limit.".into()),
            EngineError::RateLimited { .. }
        ));
        assert!(matches!(
            classify(None, "something odd".into()),
            EngineError::Other(_)
        ));
    }
}
