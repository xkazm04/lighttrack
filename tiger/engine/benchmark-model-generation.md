---
note_type: engine-call-site
call_site: benchmark-model-generation
task: Render a benchmark case through a declared model target and return a candidate with provenance
use_case: UC2
modality: text
entry: crates/runner/src/targets.rs:92
wrapper: lighttrack_engine::providers::generate or generate_deterministic
prompt_builder: crates/runner/src/targets.rs:54
output_contract: crates/engine/src/lib.rs:384-410 -> provider-specific non-empty validation
providers: [anthropic, google, openai, openrouter, codex]
model: per BenchTarget provider/model/thinking configuration
grounding: "2/2 in-direction ; out-direction closed for candidate/run provenance, subject to caller report"
dials: { wrapping: 8/10, observability: 7/10, caching: 2/10 }
fingerprint: 2029ae1d8806adc373bd957a655a64c85bd54544bb8d776464829f46ef0d5938
status: discovered
characters: ["[[maya-chen]]", "[[elena-garcia]]", "[[daniel-brooks]]"]
last_reviewed: 2026-09-15 (init; no session)
tags: [engine, llm-call-site]
---
# Engine call site - benchmark-model-generation

## What the model is asked to do

Produce one candidate answer for a frozen benchmark case under a target's provider, model, thinking
level, and resolved prompt. A prompt containing {{input}} becomes the whole user text; otherwise it
is a system instruction and the case remains a user turn. Expected answers are deliberately withheld
from generation and used only by evaluation.

## Grounding audit (Lens B) - grounding 2/2

1. Target prompt content or system_prompt - resolved and rendered at crates/runner/src/targets.rs:54-66; an unfetchable prompt reference hard-fails at :105-153.
2. Case input - substituted at the declared placeholder or sent as the user turn at :61-66.

Prompt evidence is dynamic, coming from the benchmark target/prompt registry; no target prompt or
sample output was invented at init. A live verdict must quote the resolved prompt and candidate.

Out direction uses GenOutcome: raw candidate, provider-reported model, tokens, reasoning tokens when
available, cost or honest absence, latency, determinism, and schema state. The benchmark layer also
records resolved prompt version.

## Lens A - Engine Quality dials

- Wrapping 8/10: one provider dispatch, bounded retry/timeout, typed provider adapters, empty-output
  rejection, optional deterministic pin, schema state. Generic call cancellation and global
  input/output caps need verification.
- Observability 7/10: candidate/run records can carry model, prompt version, usage, cost, latency,
  reasoning tokens, and determinism. Common prompt fingerprint/raw-request capture is not native.
- Caching 2/10: registry content is resolved once per target/run and provider clients reuse
  connections; no candidate result, prefix, or in-flight semantic cache was discovered.

## Lens C - model fit

This is the core candidate axis, so there is no global floor. Each workload uses the target matrix
in [[../models]], fixed cases, and an independent judge. Compare per-case quality with latency and
cost; multiple samples are deliberate variation and must be stamped Sampled rather than Exact.

## Findings

None. Init inventories; tiger run is required to open or certify findings.

## Strengths (do not refactor away)

Prompt version resolution fails closed, expected answers cannot leak into generation, effort travels
as a typed provider knob, and deterministic-versus-sampled intent is explicit.

Fingerprint inputs: crates/runner/src/targets.rs:54-102, crates/engine/src/chat.rs:56-77, and
crates/engine/src/providers.rs:195-208.
