---
note_type: engine-call-site
call_site: structured-rubric-judge
task: Score every LLM rubric dimension, then compute clamps, weights, floors, pass, and agreement locally
use_case: UC2
modality: text
entry: crates/engine/src/judge.rs:59
wrapper: lighttrack_engine::providers::generate_deterministic
prompt_builder: crates/engine/src/prompts.rs:246
output_contract: crates/engine/src/prompts.rs:122-157 -> crates/engine/src/judge.rs:81-121
providers: [anthropic, google, openai, openrouter, codex]
model: dynamic benchmark judge; runner default opus@xhigh (inventory 2026-09-15)
grounding: "3-4/4 in-direction ; out-direction open: rich parsed provenance, no common raw I/O ledger"
dials: { wrapping: 9/10, observability: 8/10, caching: 2/10 }
fingerprint: fdbc6e219c52d0e1e96bdcac9eb34892220ead6c8b3ebc9580e92f52ecbda58d
status: discovered
characters: ["[[maya-chen]]", "[[priya-raman]]", "[[elena-garcia]]", "[[daniel-brooks]]"]
last_reviewed: 2026-09-15 (init; no session)
tags: [engine, llm-call-site]
---
# Engine call site - structured-rubric-judge

## What the model is asked to do

Score each LLM-kind dimension from 0.0 to 1.0 against descriptions and anchors, returning one short
reason per dimension. Deterministic dimensions are evaluated locally and hidden from the model.
Local code clamps values, weights dimensions, applies gating floors, aggregates samples, and refuses
an all-unusable run.

## Grounding audit (Lens B) - grounding 3-4/4

1. LLM dimension keys, weights, descriptions, and anchors - narrated at crates/engine/src/prompts.rs:328-347.
2. User input - nonce-fenced at :253-258.
3. Candidate output - nonce-fenced at :258.
4. Reference/expected answer - fenced when present at :255-257.

Prompt evidence: score the ASSISTANT OUTPUT on EACH dimension using the anchors; penalize unnecessary
length and ignore model provenance. No real model output was sampled at init.

Out direction retains dimensions, reasonings, local overall/pass, sample coverage, agreement,
parse failures, model, usage/cost/latency, determinism, injection, and batch methodology where
applicable. Raw prompt/response capture is not universal.

## Lens A - Engine Quality dials

- Wrapping 9/10: common deterministic provider seam; dynamic strict schema; typed/clamped parse;
  single repair; nonce fences; local deterministic floor; bounded sample concurrency.
- Observability 8/10: rich outcome and honest all-failed behavior. Full I/O capture and explicit
  repair-used/per-attempt outcome remain outside the shared result.
- Caching 2/10: no semantic verdict cache, prefix caching, or in-flight dedup found; samples and
  reruns can repay identical judge context.

## Lens C - model fit

Clamps and local aggregation bound numeric swing, but anchor interpretation, reasoning, adversarial
robustness, and near-threshold ranking remain unbounded enough to differentiate models. Test a cheap
floor with repeated hostile/borderline cases; measure repeatability per dimension before treating a
premium score shift as signal.

## Findings

None. Init inventories; tiger run is required to open or certify findings.

## Strengths (do not refactor away)

One schema authority for requested dimensions; deterministic dimensions cost no tokens; values are
clamped; floors and pass are host-owned; every sample reasoning and failure count survives.

Fingerprint inputs: crates/engine/src/prompts.rs:122-157, :241-271, and :328-347.
