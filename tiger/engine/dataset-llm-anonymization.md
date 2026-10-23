---
note_type: engine-call-site
call_site: dataset-llm-anonymization
task: Replace residual free-text identifiers after deterministic dataset scrubbing while preserving meaning
use_case: UC2
modality: text
entry: crates/runner/src/dataset.rs:244
wrapper: lighttrack_engine::judge::run_text
prompt_builder: crates/runner/src/dataset.rs:238
output_contract: none -> crates/runner/src/dataset.rs:245-253 non-empty trim
providers: [anthropic-claude-cli]
model: EngineConfig model; runner default opus@xhigh when enabled
grounding: "1/3 in-direction ; out-direction open: clean text/count survive, model provenance does not"
dials: { wrapping: 5/10, observability: 3/10, caching: 1/10 }
fingerprint: 848a159682054c0bd186fd6316b74e11052effb891248b22b5d722dfbcfebf64
status: discovered
characters: ["[[priya-raman]]", "[[maya-chen]]"]
last_reviewed: 2026-09-15 (init; no session)
tags: [engine, llm-call-site]
---
# Engine call site - dataset-llm-anonymization

## What the model is asked to do

Rewrite already regex-scrubbed text, replacing remaining people, organizations, precise locations,
and account/order identifiers with typed placeholders while preserving meaning and structure. This
optional pass runs only when the operator selects LLM scrubbing.

## Grounding audit (Lens B) - grounding 1/3

1. Deterministically scrubbed source text - reaches the prompt at crates/runner/src/dataset.rs:238-244.
2. Dataset/project-specific sensitivity and retention policy - not supplied.
3. Known tenant identifiers or a review lexicon - not supplied.

Prompt evidence: replace remaining personally identifiable information with generic placeholders;
preserve meaning and structure; return only rewritten text. No sample output was captured at init,
so anonymization quality and semantic preservation are ungrounded.

Out direction retains rewritten text and an estimated added-placeholder count. TextOutcome model,
cost, and latency are discarded by scrub_text; which exact spans moved is not recorded.

## Lens A - Engine Quality dials

- Wrapping 5/10: common bounded Claude process invocation and deterministic pre-scrub precede the
  call. There is no structured span contract, semantic-preservation validator, repair, or explicit
  input/output size bound at this seam; an empty result keeps the deterministic scrubbed text.
- Observability 3/10: the call outcome has model/cost/latency, but this consumer returns only text
  and redaction count. Full prompt/output retention would itself be privacy-sensitive.
- Caching 1/10: no result/prefix/in-flight cache; privacy policy may intentionally limit caching,
  but identical corpus rebuilds can repay the same scrub.

## Lens C - model fit

The deterministic first pass bounds common patterns; detecting names/organizations while preserving
task meaning is the unbounded part. Test sonnet@low against a premium row on synthetic identifiers
and meaning-preservation checks. Cheapest is not sufficient if Priya finds leaked PII.

## Findings

None. Init inventories; tiger run is required to open or certify findings.

## Strengths (do not refactor away)

Deterministic scrub always runs first; LLM use is explicit opt-in; typed placeholders preserve
sentence structure; empty output cannot erase the case.

Fingerprint input: crates/runner/src/dataset.rs:232-255.
