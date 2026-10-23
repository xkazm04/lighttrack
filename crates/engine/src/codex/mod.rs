//! The Codex CLI (`codex exec`) as a generation path — GPT models on a ChatGPT seat.
//!
//! The counterpart of the `claude -p` path: a subscription-metered CLI rather than a metered API, so
//! a matrix can walk a GPT model's whole effort ladder without spending API money. Provider id
//! `codex`, matched on the id like OpenRouter's, because a CLI is not a lab — the model name
//! (`gpt-5.5`) is what the self-preference control reads, and that says OpenAI.
//!
//! **Isolation is the posture** ([`posture`]). A bare `codex exec` loads the operator's `config.toml`
//! (their default model and effort), their execpolicy rules and their `AGENTS.md`, any of which would
//! silently join the prompt. So every call is `--ignore-user-config --ignore-rules`, runs in a neutral
//! directory, is sandboxed `read-only`, and `OPENAI_API_KEY` / `CODEX_API_KEY` are stripped from the
//! child so a seat run cannot quietly become metered.
//!
//! **And tool-less, proven** ([`audit`]). A read-only sandbox still lets a model run code and search
//! the web, and on a reasoning benchmark that measures the wrong thing: gpt-6-astra summed a range of
//! primes with a JavaScript loop and 0 reasoning tokens, and gpt-5.5 searched the web for the list.
//! Every tool is disabled, and each turn's session log is audited afterwards; an answer a tool
//! produced is an error, never a score.
//!
//! **What it cannot give, stated:** no dollar cost (a seat call has none to report), so these targets
//! are unpriced and sit off the cost–quality frontier by name; and no sampling knobs, so `best-effort`
//! determinism. With tools off a call carries roughly 6–9k input tokens of Codex's own instructions —
//! the ~13k first measured was mostly tool definitions.

mod audit;
mod events;
mod posture;
mod resolve;

use std::path::PathBuf;
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

fn bin() -> &'static str {
    static BIN: OnceLock<String> = OnceLock::new();
    BIN.get_or_init(resolve_codex_bin)
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

/// The wall clock for one call. `LIGHTTRACK_CODEX_TIMEOUT_SECS` overrides both defaults.
///
/// Tool-free reasoning at scale is slow: a low-effort gpt-6-astra call counting primes in a
/// 3000-wide interval ran past 600s in the v3 pilot. A timed-out cell is a *generation failure* to the
/// compare runner, and three in a row open that target's breaker — so on a corpus whose hard cases run
/// back to back, a ceiling set for easy work would silently prune a max-effort target's hard tier.
/// An operator running such a corpus raises the ceiling; a value that does not parse to a positive
/// number of seconds is ignored rather than trusted.
fn timeout_for(effort: Option<Effort>, override_secs: Option<String>) -> Duration {
    if let Some(secs) = override_secs
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
    {
        return Duration::from_secs(secs);
    }
    match effort {
        Some(Effort::XHigh) | Some(Effort::Max) => TIMEOUT_HIGH_EFFORT,
        _ => TIMEOUT,
    }
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
        // Codex gave up reconnecting a dropped stream: a transport failure, and worth another try.
        None if lower.contains("stream disconnected") || lower.contains("reconnecting") => {
            EngineError::Timeout { who }
        }
        None => EngineError::Other(format!("{WHO}: {message}")),
    }
}

/// A system prompt too long for the argv path, folded into the user turn instead.
///
/// `developer_instructions` travels on the command line, and Windows caps that near 32k
/// characters, so [`posture::MAX_INSTRUCTIONS_CHARS`] is a hard limit of the transport, not a
/// preference. Refusing the call was the first answer, and it turned every Codex attempt on an
/// app with a 22k-character system prompt into a failed row (ascent, 0/21). Folding is what an
/// operator does by hand in that case; doing it here means the same prompt reaches the model
/// either way — as instructions when it fits, as the head of the user turn when it does not —
/// and the outcome is a measured answer rather than a transport error. The fold is announced on
/// stderr because it *is* a different shape, and a matrix comparing Codex rows against rows that
/// took the prompt as a system turn should know.
fn fold_oversized_system<'a>(
    system_prompt: Option<&'a str>,
    input: &'a str,
) -> (Option<&'a str>, std::borrow::Cow<'a, str>) {
    match system_prompt {
        Some(sys) if sys.chars().count() > posture::MAX_INSTRUCTIONS_CHARS => {
            eprintln!(
                "[engine] codex: system prompt is {} chars, over the {}-char argv cap; folded \
                 into the user turn",
                sys.chars().count(),
                posture::MAX_INSTRUCTIONS_CHARS
            );
            let folded = format!(
                "<instructions>\n{sys}\n</instructions>\n\nFollow the instructions above for the \
                 request below.\n\n{input}"
            );
            (None, std::borrow::Cow::Owned(folded))
        }
        other => (other, std::borrow::Cow::Borrowed(input)),
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
    let (system_prompt, input) = fold_oversized_system(system_prompt, input);
    let input: &str = &input;
    let schema_file = schema.map(write_schema).transpose()?;
    let args = posture::argv(model, system_prompt, effort, schema_file.as_deref())?;
    let mut cmd = Command::new(bin());
    cmd.args(&args)
        .current_dir(neutral_cwd()?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    strip_api_keys(&mut cmd);
    let timeout = timeout_for(effort, std::env::var("LIGHTTRACK_CODEX_TIMEOUT_SECS").ok());

    let started = Instant::now();
    let spawned = spawn_bounded(cmd, input, timeout, bin(), WHO);
    let latency_ms = Some(started.elapsed().as_millis() as u64);
    if let Some(f) = &schema_file {
        let _ = std::fs::remove_file(f);
    }
    let (status, stdout, stderr) = spawned?;

    let turn = events::read(&String::from_utf8_lossy(&stdout));
    // The CLI's own report of what went wrong outranks its exit code: it is the only place the API's
    // message ("requires a newer version of Codex") survives. A failed turn answered nothing, so its
    // log is discarded rather than audited.
    // Only a turn that never completed reports its error: one that completed answered, whatever it
    // narrated on the way there.
    if turn.usage.is_none() {
        if let Some(message) = turn.error {
            audit::discard(turn.thread_id.as_deref());
            return Err(classify(turn.status, message));
        }
    }
    if !status.success() {
        audit::discard(turn.thread_id.as_deref());
        return Err(EngineError::NonZero {
            code: status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
        });
    }
    // Before the answer is believed: prove no tool produced it.
    audit::verify_and_discard(turn.thread_id.as_deref())?;
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

    #[test]
    fn an_oversized_system_prompt_is_folded_into_the_user_turn_and_a_fitting_one_is_not() {
        let huge = "x".repeat(posture::MAX_INSTRUCTIONS_CHARS + 1);
        let (sys, input) = fold_oversized_system(Some(&huge), "the case");
        assert!(sys.is_none());
        assert!(input.starts_with("<instructions>\n"));
        assert!(input.ends_with("the case"));
        assert!(input.contains(&huge));

        let (sys, input) = fold_oversized_system(Some("terse"), "the case");
        assert_eq!(sys, Some("terse"));
        assert_eq!(&*input, "the case");

        let (sys, input) = fold_oversized_system(None, "the case");
        assert!(sys.is_none());
        assert_eq!(&*input, "the case");
    }

    #[test]
    fn the_timeout_scales_with_effort_and_the_override_wins_when_it_parses() {
        assert_eq!(timeout_for(Some(Effort::Low), None), TIMEOUT);
        assert_eq!(timeout_for(Some(Effort::Max), None), TIMEOUT_HIGH_EFFORT);
        assert_eq!(timeout_for(None, None), TIMEOUT);
        assert_eq!(
            timeout_for(Some(Effort::Low), Some("2400".into())),
            Duration::from_secs(2400),
            "the override covers every level"
        );
        assert_eq!(
            timeout_for(Some(Effort::Max), Some(" 3600 ".into())),
            Duration::from_secs(3600)
        );
        for junk in ["", "0", "-5", "forever"] {
            assert_eq!(
                timeout_for(Some(Effort::Low), Some(junk.into())),
                TIMEOUT,
                "{junk:?} is not a ceiling"
            );
        }
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
            classify(
                None,
                "stream disconnected before completion: WebSocket protocol error".into()
            ),
            EngineError::Timeout { .. }
        ));
        assert!(matches!(
            classify(None, "something odd".into()),
            EngineError::Other(_)
        ));
    }
}
