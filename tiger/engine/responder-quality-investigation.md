---
note_type: engine-call-site
call_site: responder-quality-investigation
task: Diagnose a production output-quality regression against a mapped repository using read-only tools
use_case: UC1
modality: text
entry: crates/responder/src/invoke.rs:52
wrapper: lighttrack_engine::invocation::run in ReadonlyScan mode
prompt_builder: crates/responder/src/investigate.rs:66
output_contract: none; section headings requested in prompt
providers: [anthropic-claude-cli]
model: responder default sonnet, configurable in map
grounding: "4/4 in-direction ; out-direction open: diagnosis/model/cost/status survive, tokens/latency/prompt identity do not"
dials: { wrapping: 8/10, observability: 6/10, caching: 3/10 }
fingerprint: 6d590163de820c065d6b7ba2577c61ce4ad9b51ee1ae6ecf58756c26bd51c0c1
status: discovered
characters: ["[[tomas-novak]]", "[[priya-raman]]", "[[daniel-brooks]]"]
last_reviewed: 2026-09-15 (init; no session)
tags: [engine, llm-call-site]
---
# Engine call site - responder-quality-investigation

## What the model is asked to do

Inspect a repository read-only and explain a judged score drop as a likely prompt/template,
model/parameter, retrieval/context, or input/output-shaping change. It must cite file:line, propose
a remedy with risks, and state confidence.

## Grounding audit (Lens B) - grounding 4/4

1. Project/repository and area hint - crates/responder/src/investigate.rs:67-76.
2. Rubric, judge, drop percentage, recent mean, and baseline - :68-78.
3. Recent judged scores with judge reasoning - :79-80.
4. Repository history/source - scoped read-only working directory and tools at :12-32.

Prompt evidence explicitly says this is output quality, not a crash. Judge reasoning is labelled
untrusted, although unlike error_prompt it is not wrapped in explicit begin/end markers. No live
diagnosis output was sampled at init.

Out direction retains diagnosis/model/cost/status but not token/latency/prompt identity in
ClaudeRun. Prior baseline methodology and raw target outputs depend on upstream enrichment.

## Lens A - Engine Quality dials

- Wrapping 8/10: same centralized read-only posture, budget, timeout, and admission as error
  investigation; unstructured output lacks schema/validation/repair.
- Observability 6/10: reportable diagnosis and cost/model/status; token/latency/fingerprint and
  prompt/raw response audit trail are incomplete at this seam.
- Caching 3/10: cooldown/caps/dedup reduce alert storms; no semantic result or provider prefix cache.

## Lens C - model fit

This is unbounded causal reasoning over sparse score context plus a repository. Compare models on a
planted prompt/model/retrieval change. A premium row earns its cost only by finding the correct
change with defensible evidence and avoiding a confident wrong cause.

## Findings

None. Init inventories; tiger run is required to open or certify findings.

## Strengths (do not refactor away)

Quality drops have a purpose-built prompt, measured baseline context, read-only posture, confidence
request, and a separate mutation gate.

Fingerprint input: crates/responder/src/investigate.rs:65-90.
