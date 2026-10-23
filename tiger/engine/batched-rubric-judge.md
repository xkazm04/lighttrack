---
note_type: engine-call-site
call_site: batched-rubric-judge
task: Judge multiple independent rubric cases in one provider call without losing per-case attribution
use_case: UC2
modality: text
entry: crates/engine/src/judge.rs:59
wrapper: lighttrack_engine::providers::generate_deterministic
prompt_builder: crates/engine/src/prompts.rs:292
output_contract: crates/engine/src/prompts.rs:159-187 -> crates/engine/src/judge/batch.rs:50-86
providers: [anthropic, google, openai, openrouter, codex]
model: dynamic benchmark judge; runner default opus@xhigh (inventory 2026-09-15)
grounding: "4-5/5 in-direction ; out-direction open: per-case provenance retained, raw shared call not universal"
dials: { wrapping: 9/10, observability: 8/10, caching: 5/10 }
fingerprint: 48f80e0b1bae590ab65add2e842cb9461a79e7329562dafd3b0705154fc1c629
status: discovered
characters: ["[[maya-chen]]", "[[priya-raman]]", "[[elena-garcia]]"]
last_reviewed: 2026-09-15 (init; no session)
tags: [engine, llm-call-site]
---
# Engine call site - batched-rubric-judge

## What the model is asked to do

Apply one anchored rubric independently to N cases and return exactly one verdict per stable case id.
The prompt forbids cross-case ranking; samples rotate presentation order. The decoder matches ids
rather than positions, splits cost/tokens across cases, and stamps batch_size because batched and
single scores are different methodologies.

## Grounding audit (Lens B) - grounding 4-5/5

1. Stable case id - prompt heading and required output field at crates/engine/src/prompts.rs:297-318.
2. Rubric dimension keys, weights, descriptions, and anchors - :293 and :328-347.
3. Each case input - separately nonce-fenced at :299.
4. Each candidate output - separately nonce-fenced at :303.
5. Each optional reference - separately fenced when present at :300-302.

Prompt evidence: cases are unrelated, must be scored independently, and case order is arbitrary.
No live output exists in the vault, so Lens B quality remains ungrounded.

## Lens A - Engine Quality dials

- Wrapping 9/10: common schema/retry/timeout/repair seam; ids never zip by position; missing,
  duplicate, or invented ids cannot claim another case; nonce collision taints the whole batch.
- Observability 8/10: batch size, amortized tokens/cost, whole-call latency, case failures,
  agreement, determinism, and injection survive. Shared raw call capture is not universal.
- Caching 5/10: batching is a material context-amortization mechanism. It is not a result cache,
  provider prefix cache, or in-flight dedup, and batch-size effects must not be called equivalent.

## Lens C - model fit

Batching lowers overhead but creates anchoring/context-pressure risk. Hold cases fixed, rotate order,
and compare single versus batch before choosing the largest cheap batch. Model quality is not
measured at init; predicted cheap floors need agreement and attribution tests.

## Findings

None. Init inventories; tiger run is required to open or certify findings.

## Strengths (do not refactor away)

Stable ids, case-order rotation, explicit independence instructions, per-block nonce fences,
per-case failure isolation, shared single-case parser, and visible methodology/accounting stamps.

Fingerprint inputs: crates/engine/src/prompts.rs:135-187 and :281-347.
