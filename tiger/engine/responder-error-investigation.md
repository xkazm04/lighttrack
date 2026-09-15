---
note_type: engine-call-site
call_site: responder-error-investigation
task: Diagnose a production LLM error against a mapped repository using read-only tools
use_case: UC1
modality: text
entry: crates/responder/src/invoke.rs:52
wrapper: lighttrack_engine::invocation::run in ReadonlyScan mode
prompt_builder: crates/responder/src/investigate.rs:37
output_contract: none; section headings requested in prompt
providers: [anthropic-claude-cli]
model: responder default sonnet, configurable in map
grounding: "4/4 in-direction ; out-direction open: diagnosis/model/cost/status survive, tokens/latency/prompt identity do not"
dials: { wrapping: 8/10, observability: 6/10, caching: 3/10 }
fingerprint: a442a485d82232dc99ba94132b75c21a5b6246ce4311c07960da28a40c0ae5ac
status: discovered
characters: ["[[tomas-novak]]", "[[priya-raman]]", "[[daniel-brooks]]"]
last_reviewed: 2026-09-15 (init; no session)
tags: [engine, llm-call-site]
---
# Engine call site - responder-error-investigation

## What the model is asked to do

Inspect a mapped repository read-only, find the path producing a production LLM failure, identify
the most likely root cause, and propose a concrete file-level fix with risks and confidence.

## Grounding audit (Lens B) - grounding 4/4

1. Project/repository context, area hint, and configured verify command - prompt lines
   crates/responder/src/investigate.rs:38-48.
2. Spike count/status/model and latest error - :40-51.
3. Recent failing events - :52-54.
4. Repository history/source - available through a scoped read-only working directory and tools at :12-32.

Prompt evidence: find the code path, determine root cause, propose a concrete fix, and cite
file:line with confidence. Error and event strings are labelled untrusted and surrounded by fixed
markers. No diagnosis sample was captured at init.

Out direction retains diagnosis text, model, optional cost, and success for the pipeline/report.
ClaudeRun does not carry token or latency fields, and the prompt fingerprint is not recorded.

## Lens A - Engine Quality dials

- Wrapping 8/10: one invocation seam; explicit ReadonlyScan posture; scoped git read tools; default
  $1 budget, 240-second reaping timeout, cooldown/hourly/concurrency admission, and controlled error
  envelope. The prose has no typed output validator or repair.
- Observability 6/10: diagnosis, model, money, status, report path, and alert resolution survive;
  token/latency/prompt identity and full per-attempt evidence are narrower than the engine outcome.
- Caching 3/10: project cooldown, hourly cap, and concurrency shedding reduce repeat spend. There is
  no semantic diagnosis cache or prompt prefix cache.

## Lens C - model fit

Repo root-cause analysis is unbounded and evidence quality—not prose length—sets the floor.
Sonnet@medium is predicted; compare higher effort only on planted causal diffs and score exact
evidence, confidence calibration, and safe remedy under the same timeout/budget.

## Findings

None. Init inventories; tiger run is required to open or certify findings.

## Strengths (do not refactor away)

Read-only mode is structural, write-capable tools are rejected, failure text is explicitly
untrusted, budget/timeout and admission are bounded, and mutation is separated into another call site.

Fingerprint input: crates/responder/src/investigate.rs:35-63.
