---
note_type: engine-call-site
call_site: benchmark-healing-recommendations
task: Turn benchmark aggregate weaknesses into three to five concrete improvement bullets
use_case: UC2
modality: text
entry: crates/runner/src/rubric.rs:369
wrapper: lighttrack_engine::judge::run_text
prompt_builder: crates/runner/src/rubric.rs:361
output_contract: none
providers: [anthropic-claude-cli]
model: EngineConfig model; runner default opus@xhigh when heal is enabled
grounding: "4/5 in-direction ; out-direction open: prose persists, call provenance is not attached"
dials: { wrapping: 4/10, observability: 3/10, caching: 1/10 }
fingerprint: c80212b82eac9eb76c1c2e213cf705ea053aa8068301af0f6d431b8b0e8ee797
status: discovered
characters: ["[[maya-chen]]", "[[elena-garcia]]", "[[daniel-brooks]]"]
last_reviewed: 2026-09-15 (init; no session)
tags: [engine, llm-call-site]
---
# Engine call site - benchmark-healing-recommendations

## What the model is asked to do

Act as an evaluation consultant and recommend prompt, model, or rubric changes targeting weak
dimensions in three to five concise bullets. The operator must explicitly enable heal.

## Grounding audit (Lens B) - grounding 4/5

1. Benchmark name - included at crates/runner/src/rubric.rs:363-368.
2. Mean overall and threshold - included.
3. Pass rate and failed-case count - included.
4. Per-dimension keys, weights, and means - assembled at :348-360.
5. Representative failed cases and judge reasoning - not supplied.

Prompt evidence: recommend concrete fixes targeting the weakest dimensions. No model output exists
in this vault; senior specificity is ungrounded and needs L2. The prose is added to JSON/console
report, but model, cost, latency, and prompt identity are not attached to the healing field.

## Lens A - Engine Quality dials

- Wrapping 4/10: bounded Claude subprocess and explicit opt-in exist. Output is unstructured prose
  with no validator, repair, bullet-count check, quality gate, or explicit input/output bound.
  Failure degrades to no healing paragraph and logs the error.
- Observability 3/10: run_text returns model/cost/latency, but the consumer stores only trimmed text
  and a failure message goes to stderr.
- Caching 1/10: no result, prefix, or in-flight cache was discovered.

## Lens C - model fit

This is unbounded diagnosis over highly compressed aggregates. Sonnet@low is the predicted floor,
but every model may remain generic without failed exemplars; if so, improve grounding rather than
upgrade. Premium value must move Maya/Daniel specificity, not bullet polish.

## Findings

None. Init inventories; tiger run is required to open or certify findings.

## Strengths (do not refactor away)

Explicit heal opt-in; the prompt names the measured aggregates; failure cannot rewrite the benchmark
verdict; generated advice is visibly separate from significance logic.

Fingerprint input: crates/runner/src/rubric.rs:346-375.
