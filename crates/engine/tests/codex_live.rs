//! The Codex path against the **real** `codex` binary, through the real engine dispatch.
//!
//! Ignored by default: it spends seat usage and needs a logged-in Codex CLI, so it can never be a
//! gate (the same reasoning that keeps live-provider agreement out of `judge-eval`). Run it on
//! purpose, after touching the adapter or upgrading Codex:
//!
//! `cargo test -p lighttrack-engine --test codex_live -- --ignored --nocapture`
//!
//! It exists because the unit tests can only replay captured output — and the one other Codex reader
//! in the wider codebase was built against a fixture its own header calls synthetic, never checked
//! against the binary. A reader is only as good as the last time it met the real thing.

use lighttrack_engine::{generate, EngineConfig, EngineError};

/// The whole path: dispatch on the `codex` id, the `@effort` split, the isolation flags, the system
/// prompt through `developer_instructions`, and the reasoning split read off the event stream.
#[test]
#[ignore = "spends Codex seat usage; run with --ignored"]
fn codex_generates_through_the_engine_with_effort_and_a_reasoning_split() {
    let out = generate(
        &EngineConfig::default(),
        "codex",
        "gpt-5.5@high",
        Some("Reply with only the final answer: a single integer, no words."),
        // 7560 = 2^3 · 3^3 · 5 · 7, so (3+1)(3+1)(1+1)(1+1) = 64 divisors.
        "How many positive divisors does 7560 have?",
        None,
    )
    .expect("a logged-in codex CLI generates");
    eprintln!("{out:#?}");
    assert_eq!(out.output.trim(), "64", "the answer, and only the answer");
    assert_eq!(
        out.model, "gpt-5.5",
        "the effort suffix never reaches the model id"
    );
    let output = out.output_tokens.expect("usage is read");
    let reasoning = out.reasoning_tokens.expect("the reasoning split is read");
    assert!(
        reasoning <= output,
        "reasoning is inside output, never added to it"
    );
    assert!(out.cost_usd.is_none(), "a seat call reports no dollars");
}

/// **The case a model would script.** gpt-6-astra first answered this with a JavaScript loop and 0
/// reasoning tokens, and gpt-5.5 with a web search. With tools disabled and the session log audited,
/// it either reasons its way to an answer or the adapter refuses to score it — an `Ok` here is the
/// proof the answer did not come from a tool.
#[test]
#[ignore = "spends Codex seat usage; run with --ignored"]
fn a_computation_the_model_would_script_is_answered_without_any_tool() {
    let out = generate(
        &EngineConfig::default(),
        "codex",
        "gpt-6-astra@low",
        Some("Reply with only the final answer: a single integer, no words."),
        "What is the sum of all prime numbers between 10000 and 10500?",
        None,
    )
    .expect("answered, and the audit found no tool that ran");
    eprintln!("{out:#?}");
    assert!(
        out.reasoning_tokens.unwrap_or(0) > 0,
        "an answer to this with no reasoning at all is what a tool call looks like"
    );
}

/// A model the seat cannot use fails loudly with the API's own words, not an empty completion.
#[test]
#[ignore = "spends Codex seat usage; run with --ignored"]
fn a_model_codex_refuses_surfaces_the_apis_reason() {
    let err = generate(
        &EngineConfig::default(),
        "codex",
        "gpt-definitely-not-a-model-zz",
        None,
        "Say ok.",
        None,
    )
    .expect_err("an unknown model is refused");
    eprintln!("{err}");
    assert!(
        !matches!(err, EngineError::EmptyCompletion { .. }),
        "a refusal must never read as a model that said nothing: {err}"
    );
}
