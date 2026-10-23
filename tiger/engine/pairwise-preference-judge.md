---
note_type: engine-call-site
call_site: pairwise-preference-judge
task: Choose between two candidate answers with A/B order counterbalancing and a real tie state
use_case: UC2
modality: text
entry: crates/engine/src/pairwise.rs:165
wrapper: lighttrack_engine::providers::generate_deterministic
prompt_builder: crates/engine/src/prompts.rs:195
output_contract: crates/engine/src/prompts.rs:228-238 -> crates/engine/src/pairwise.rs:77-81
providers: [anthropic, google, openai, openrouter, codex]
model: dynamic comparison judge; runner default opus@xhigh (inventory 2026-09-15)
grounding: "3-5/5 in-direction ; out-direction open: combined verdict retained, both raw order outputs are not universal"
dials: { wrapping: 9/10, observability: 8/10, caching: 2/10 }
fingerprint: 41b88de8963d9426f64ad1034f05be06e62e1aa7ec193b073c41ee32d27a9df6
status: discovered
characters: ["[[maya-chen]]", "[[priya-raman]]", "[[elena-garcia]]"]
last_reviewed: 2026-09-15 (init; no session)
tags: [engine, llm-call-site]
---
# Engine call site - pairwise-preference-judge

## What the model is asked to do

Choose A, B, or Tie on content merit. The engine makes two paid calls with answer order reversed,
maps the second verdict back to caller order, and turns disagreement into Tie plus position_bias.

## Grounding audit (Lens B) - grounding 3-5/5

1. Input - always nonce-fenced at crates/engine/src/prompts.rs:202-208.
2. Answer A - always separately fenced.
3. Answer B - always separately fenced.
4. Reference/expected answer - fenced when present at :204-206.
5. Evaluation criteria - included when non-empty at :209-212.

Prompt evidence: decide on merit of content, ignore style/tone/length/formatting/provenance, and do
not let A/B order influence the decision. No model output was sampled at init.

## Lens A - Engine Quality dials

- Wrapping 9/10: deterministic schema-backed calls, nonce fences, repair, typed enum, explicit tie,
  and structural order counterbalance.
- Observability 8/10: combined outcome carries position bias, reasoning, model, usage/cost,
  determinism, and injection. When orders agree only the first reasoning is retained in the outcome;
  universal raw order capture was not discovered.
- Caching 2/10: both order calls are intentionally distinct, but identical reruns have no result
  cache or provider prefix cache.

## Lens C - model fit

Forced-choice content judgment is model-sensitive even though the output is tiny. Test equal,
verbosity-only, close, and clear pairs. A cheap floor clears only if both orders remain stable and an
independent judge/human agrees; premium prose length is not value.

## Findings

None. Init inventories; tiger run is required to open or certify findings.

## Strengths (do not refactor away)

Two-order measurement, honest tie on disagreement, typed winner vocabulary, cost/token sum across
both calls, and nonce protection for input/reference/both answers.

Fingerprint input: crates/engine/src/prompts.rs:192-239.
