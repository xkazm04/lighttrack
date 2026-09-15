---
note_type: engine-call-site
call_site: device-relay-action
task: Render and execute a device-local action against a leased task under declared posture and schema
use_case: UC4
modality: text
entry: crates/agent/src/exec.rs:73
wrapper: lighttrack_engine::invocation::run
prompt_builder: crates/agent/src/actions.rs:145
output_contract: optional action schema -> crates/agent/src/exec.rs:92-101 JSON syntax validation
providers: [anthropic-claude-cli]
model: per action; default sonnet
grounding: "4/4 in-direction ; out-direction closed for status/provenance with content private by default"
dials: { wrapping: 9/10, observability: 8/10, caching: 2/10 }
fingerprint: cdf8ba2ce0c5e7bee07ae435f7088633719681fd59069a79fd155b98e4c026a6
status: discovered
characters: ["[[tomas-novak]]", "[[priya-raman]]", "[[daniel-brooks]]"]
last_reviewed: 2026-09-15 (init; no session)
tags: [engine, llm-call-site]
---
# Engine call site - device-relay-action

## What the model is asked to do

Execute a locally installed action. The cloud supplies only action_type and task payload; the device
loads action.toml, prompt.md, and optional schema.json, resolves a device-owned workspace, renders
declared placeholders, and invokes generate, read-only scan, or edit posture.

## Grounding audit (Lens B) - grounding 4/4

1. Device-owned prompt template and optional system instruction - loaded at crates/agent/src/actions.rs:145-174.
2. Complete task payload plus task/action ids - declared render vocabulary at :177-225.
3. Optional output schema - loaded locally at :163-169 and passed at crates/agent/src/exec.rs:61-68.
4. Action posture, tools, workspace, budget, timeout, model, and version - action spec at
   crates/agent/src/actions.rs:19-90 and enforced before spend at crates/agent/src/exec.rs:48-73.

Prompt evidence is action-library content outside version control here, so no representative prompt
or output is fabricated. Missing placeholders fail before a paid run at actions.rs:181-207.

Out direction carries status, parsed/wrapped result, model, tokens, latency, cost, mode, action
version, and rendered-prompt SHA-256. Full rendered prompt and raw result text leave the device only
when per-action report_io is true.

## Lens A - Engine Quality dials

- Wrapping 9/10: one invocation seam; action/path/workspace validation; explicit mode/tool/permission
  posture; missing-variable refusal; timeout/budget; optional schema request; controlled result and
  connector failure. Post-call schema handling verifies JSON syntax; semantic schema parity relies
  on the CLI enforcement.
- Observability 8/10: rich run report plus prompt fingerprint/version and privacy-default content
  omission. Per-attempt tool trace and child-observed posture evidence are separate concerns.
- Caching 2/10: no action result/prefix/in-flight cache; lease/settle semantics prevent ordinary
  duplicate ownership but connector retry can rerun work and therefore requires idempotence.

## Lens C - model fit

The action library defines the task, so optimize per action. Structured extraction may clear on a
small model; repository editing or nuanced device workflows may require sonnet or higher. Hold
payload, template/schema/version, workspace revision, and posture fixed; privacy must remain identical
across cells.

## Findings

None. Init inventories; tiger run is required to open or certify findings.

## Strengths (do not refactor away)

Cloud cannot choose paths or executable content; placeholder absence fails loudly; prompt identity
is computed before spawn; report_io is off by default; non-success envelopes cannot settle succeeded;
connector failure remains failure.

Fingerprint inputs: crates/agent/src/actions.rs:145-227 and crates/agent/src/exec.rs:17-101.
