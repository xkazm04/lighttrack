---
note_type: engine-call-site
call_site: structured-verdict-repair
task: Re-ask once for valid structured output after an empty or malformed judge response
use_case: cross
modality: text
entry: crates/engine/src/parse.rs:145
wrapper: inherited provider generator through sample_parsed
prompt_builder: crates/engine/src/prompts.rs:110
output_contract: inherited caller schema -> crates/engine/src/parse.rs:145-159
providers: [anthropic, google, openai, openrouter, codex]
model: inherited from the original judge call
grounding: "2/2 in-direction ; out-direction open: accounting survives, repair use is not a first-class result field"
dials: { wrapping: 8/10, observability: 7/10, caching: 1/10 }
fingerprint: 5addd202f3dabc936e3cdccbed4a52edd1f501fbf1d34a3fc9238963e0de4b16
status: discovered
characters: ["[[maya-chen]]", "[[priya-raman]]"]
last_reviewed: 2026-09-15 (init; no session)
tags: [engine, llm-call-site]
---
# Engine call site - structured-verdict-repair

## What the model is asked to do

Re-read the original prompt and its own rejected output, then return only valid JSON in the original
shape. There is exactly one repair generation. Failure remains a dropped sample or a hard all-failed
error, never a fabricated score.

## Grounding audit (Lens B) - grounding 2/2

1. Original fully rendered prompt - included at crates/engine/src/prompts.rs:113-118.
2. Previous malformed/empty model output - wrapped as untrusted data under a fresh nonce at :111-119.

Prompt evidence: the previous response was not valid JSON; return only valid JSON matching the
schema. The schema is carried by the same generator closure rather than duplicated in repair prose.
No repair output was sampled during init.

Out direction is open: tokens, cost, latency, model, determinism, injection, and final parse state
are accumulated, but a successfully repaired sample does not expose an explicit repair-used count
in Parsed or RubricOutcome.

## Lens A - Engine Quality dials

- Wrapping 8/10: one-shot bounded repair, same provider/schema path, fresh fence, typed caller parse,
  and honest terminal failure. Total provider-attempt budget and disconnect cancellation inherit
  the common wrapper questions.
- Observability 7/10: both calls are included in aggregate usage/cost; last raw failure is retained
  for an all-failed error. Successful repair and per-attempt raw bodies are not first-class output.
- Caching 1/10: a repair must be fresh, but unchanged failed packets have no semantic result cache;
  provider prefix caching is not marked.

## Lens C - model fit

This is bounded syntax recovery. It should use the same model first to avoid a hidden methodology
change; benchmark value is repair success, false semantic acceptance, added latency, and cost—not
prose quality. A larger model is justified only if it materially recovers valid, semantically correct
verdicts.

## Findings

None. Init inventories; tiger run is required to open or certify findings.

## Strengths (do not refactor away)

Only one re-ask; malformed model text is treated as hostile; every paid call is accounted; an
unrepaired response never becomes zero or success.

Fingerprint inputs: crates/engine/src/prompts.rs:105-120 and crates/engine/src/parse.rs:111-159.
