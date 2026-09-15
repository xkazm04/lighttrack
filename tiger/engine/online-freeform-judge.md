---
note_type: engine-call-site
call_site: online-freeform-judge
task: Score one observed or calibration output against an operator-authored freeform rubric
use_case: UC1
modality: text
entry: crates/engine/src/judge.rs:156
wrapper: lighttrack_engine::providers::generate_deterministic
prompt_builder: crates/engine/src/prompts.rs:53
output_contract: crates/core/src/score.rs:8-35 -> crates/engine/src/judge.rs:161-176
providers: [anthropic, google, openai, openrouter, codex]
model: dynamic runner judge setting; runner default opus@xhigh (inventory 2026-09-15)
grounding: "3/3 in-direction ; out-direction open: caller-specific persistence, no raw prompt ledger"
dials: { wrapping: 9/10, observability: 7/10, caching: 2/10 }
fingerprint: cac8f280ff12ba4a4db0a66435bf36ea740cc0f29ccf77d8f6e897016508a024
status: discovered
characters: ["[[tomas-novak]]", "[[priya-raman]]", "[[elena-garcia]]"]
last_reviewed: 2026-09-15 (init; no session)
tags: [engine, llm-call-site]
---
# Engine call site - online-freeform-judge

## What the model is asked to do

Return score, max, pass, and one-sentence reasoning for one input/output pair under a freeform
rubric. The prompt says it is a strict evaluation judge and requires only a JSON object with a
0.0-1.0 score. The typed decoder validates shape but does not locally reconcile pass with score or
an operator threshold.

Consumers share this family through crates/runner/src/calibrate.rs:322-323 and
crates/runner/src/judge_spec.rs:101-102.

## Grounding audit (Lens B) - grounding 3/3

1. Operator-authored rubric - reaches the instruction channel at crates/engine/src/prompts.rs:65.
2. User input - nonce-fenced as untrusted data at crates/engine/src/prompts.rs:55-60.
3. Candidate output - separately nonce-fenced at the same builder.

Prompt evidence: Evaluate the ASSISTANT OUTPUT for the given USER INPUT against the rubric.
Out direction is open: JudgeOutcome carries provenance, but this family has no universal durable
raw-prompt/raw-response ledger. No real sampled output was captured at init, so Lens B quality is
ungrounded and needs L2.

## Lens A - Engine Quality dials

- Wrapping 9/10: one provider dispatch; typed schema; deterministic request; nonce fencing;
  bounded transient retry; empty/malformed repair; hard failure after repair. Per-attempt timeout
  exists, but cancellation and a strict total budget inclusive of in-flight attempt time need a run.
- Observability 7/10: model, cost, latency, tokens, determinism, injection signal, and repair-call
  accounting survive. There is no common prompt/raw-response capture or explicit repaired flag.
- Caching 2/10: cached-token pricing can be represented, but no result cache, provider prefix cache,
  or identical in-flight dedup was discovered.

## Lens C - model fit

The scalar and JSON shape are bounded; interpreting an arbitrary rubric and hostile candidate is
the model-sensitive part. Haiku@medium and gpt-5.4-mini@medium are predicted floor challengers;
opus@xhigh is the current runner ceiling candidate. No quality/cost swing is measured.

## Findings

None. Init inventories; tiger run is required to open or certify findings.

## Strengths (do not refactor away)

Fresh nonce fences and a durable injection flag; deterministic provider request; one bounded repair;
typed verdict parsing; full cost/token accounting across the repair.

Fingerprint inputs: crates/engine/src/prompts.rs:53-72 and crates/core/src/score.rs:5-35 under
tiger-source-slices-v1 in [[../README]].
