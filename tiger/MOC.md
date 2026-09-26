---
note_type: moc
updated: 2026-09-15
tags: [moc, home]
---
# Tiger vault - Map of Content

## The engine

- [[online-freeform-judge]] - score production/calibration output; crates/engine/src/judge.rs:156; W9/O7/C2; grounding 3/3; UC1
- [[benchmark-freeform-evaluator]] - score a benchmark answer with optional reference; crates/engine/src/judge.rs:156; W9/O7/C2; grounding 3-4/4; UC2
- [[structured-rubric-judge]] - score anchored dimensions and aggregate locally; crates/engine/src/judge.rs:59; W9/O8/C2; grounding 3-4/4; UC2
- [[batched-rubric-judge]] - score independent cases in one call; crates/engine/src/judge.rs:59; W9/O8/C5; grounding 4-5/5; UC2
- [[pairwise-preference-judge]] - compare two answers with order counterbalancing; crates/engine/src/pairwise.rs:165; W9/O8/C2; grounding 3-5/5; UC2
- [[structured-verdict-repair]] - repair one malformed structured verdict; crates/engine/src/parse.rs:145; W8/O7/C1; grounding 2/2; cross
- [[benchmark-model-generation]] - generate a candidate for a benchmark cell; crates/runner/src/targets.rs:92; W8/O7/C2; grounding 2/2; UC2
- [[benchmark-http-target]] - ask an operator-owned application endpoint for a candidate; crates/engine/src/http_target.rs:125; W8/O7/C1; grounding 2-3/3; UC2
- [[dataset-llm-anonymization]] - scrub residual identifiers from sampled text; crates/runner/src/dataset.rs:244; W5/O3/C1; grounding 1/3; UC2
- [[benchmark-healing-recommendations]] - turn aggregate weak dimensions into improvement bullets; crates/runner/src/rubric.rs:369; W4/O3/C1; grounding 4/5; UC2
- [[gateway-chat-completion]] - serve a routed OpenAI-compatible conversation; crates/gateway/src/chat.rs:73; W8/O8/C2; grounding 4/4; UC3
- [[responder-error-investigation]] - diagnose an observed production failure read-only; crates/responder/src/investigate.rs:23; W8/O6/C3; grounding 4/4; UC1
- [[responder-quality-investigation]] - diagnose a judged quality regression read-only; crates/responder/src/investigate.rs:23; W8/O6/C3; grounding 4/4; UC1
- [[responder-auto-fix]] - apply a minimal opt-in fix from a prior diagnosis; crates/responder/src/act.rs:85; W8/O6/C2; grounding 3/4; UC1
- [[device-relay-action]] - execute a device-local action template under declared posture; crates/agent/src/exec.rs:73; W9/O8/C2; grounding 4/4; UC4
- Expected call sites: none at init.

All dial values above are starting inventory estimates, not certified findings.

## The three lenses

- [[engine-quality]]
- [[business-value]]
- [[model-optimization]]

## Model frontier

- [[models]] - matrix, repository price snapshot, and theoretical floor/ceiling hypotheses

## Characters

- [[_roster]] - who judges what, their angle, and use-case binding

## Sessions

No sessions yet. Init inventories and scaffolds; the first session is created by tiger run.

## Backlog

- [[backlog]]
