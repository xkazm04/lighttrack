---
note_type: engine-call-site
call_site: benchmark-freeform-evaluator
task: Grade a generated benchmark answer against a freeform rubric and optional reference
use_case: UC2
modality: text
entry: crates/engine/src/judge.rs:156
wrapper: lighttrack_engine::providers::generate_deterministic
prompt_builder: crates/engine/src/prompts.rs:74
output_contract: crates/core/src/score.rs:8-35 -> crates/engine/src/judge.rs:161-176
providers: [anthropic, google, openai, openrouter, codex]
model: dynamic benchmark judge; runner default opus@xhigh (inventory 2026-09-15)
grounding: "3-4/4 in-direction ; out-direction open: verdict provenance persists, raw prompt/response varies by caller"
dials: { wrapping: 9/10, observability: 7/10, caching: 2/10 }
fingerprint: fbb0300d5df366e67a5fa665e37c9af675e30a2314e2afc7caca15305233aabb
status: discovered
characters: ["[[maya-chen]]", "[[priya-raman]]", "[[elena-garcia]]", "[[daniel-brooks]]"]
last_reviewed: 2026-09-15 (init; no session)
tags: [engine, llm-call-site]
---
# Engine call site - benchmark-freeform-evaluator

## What the model is asked to do

Return a 0.0-1.0 JSON verdict for a benchmark candidate, using an optional expected answer as a
reference. It serves legacy/freeform benchmark rubrics; structured rubric judging is a separate
prompt and contract.

Consumers include crates/runner/src/bench.rs:268-274 and :538-539.

## Grounding audit (Lens B) - grounding 3-4/4

1. Freeform rubric - always present at crates/engine/src/prompts.rs:91.
2. Benchmark input - always fenced at :81-86.
3. Candidate output - always fenced at :86.
4. Reference/expected answer - fenced when the dataset provides one at :83-85; explicitly absent otherwise.

Prompt evidence: Evaluate the ASSISTANT OUTPUT for the given USER INPUT against the rubric and,
when present, the reference answer. No sampled output was captured during init; business quality is
ungrounded and needs L2.

## Lens A - Engine Quality dials

- Wrapping 9/10: shared provider abstraction/retry/timeout, strict schema, typed decoder, one repair,
  deterministic request, empty-output rejection, and nonce fencing.
- Observability 7/10: usage, cost, latency, model, determinism, injection and repair spend are
  available; common full I/O capture and an explicit repair-used field are not.
- Caching 2/10: no result/prefix/in-flight cache was discovered. Repeated grading of the same
  immutable output and judge packet is therefore a Lens C cost axis to measure.

## Lens C - model fit

The output is bounded but correctness and reference use are model-sensitive. Compare cheap judges
on borderline, confident-wrong, verbose, and injection fixtures; use an independent family or human
spot-check. Current placement and all frontier deltas are theoretical.

## Findings

None. Init inventories; tiger run is required to open or certify findings.

## Strengths (do not refactor away)

Optional reference absence is honest; every untrusted block is independently fenced; repaired calls
remain fully accounted; schema shedding and determinism are values, not log-only claims.

Fingerprint inputs: crates/engine/src/prompts.rs:74-103 and crates/core/src/score.rs:5-35.
