# Registry coverage observations - challenge-2026-10-05

## device-relay-agent: no declared join covers the context's actual hard problem
Scout read every declared subject's opening paragraph. `lt-agent` is a polling device daemon,
not an LLM agent, so these declared joins are **confirmed homonyms** and no technique was applied:
`voice-io`, `agent-browser-control` (516), `agent-memory` (801), `agent-instruction-files` (805),
`agent-runtime-assembly` (577).

Two the overlay listed as "most likely real" are **also homonyms**:
- `fleet-orchestration` (482) - about fleets of autonomous *agent sessions*
- `machine-paced-delivery` (500) - about CI/delivery practice under agent-authored change rates

`prompt-assembly` (631) is a **partial** real join: governs `actions.rs::render` + `report.rs`
(template substitution, unresolved-placeholder failure, rendered-prompt fingerprinting) and
nothing in the polling/lease machinery.
`agent-cli-transport` (721) is the ONE genuinely governing declared subject, and it governs
`exec.rs` / the engine probe, not the relay protocol.

**The subjects that actually govern a fenced lease queue are undeclared or declared only on the
rider**: `delivery-guarantees`, `retry-backoff`, `background-jobs/job-progress-and-cancellation`,
`job-coordination/job-observability`. Two of them name this repo's bugs verbatim:
- `retry-backoff / error-classification-for-retry`: "message matching is classification by
  superstition" - exactly `cloud.rs:157`'s `contains("HTTP 409")`.
- `background-jobs / job-progress-and-cancellation`: "what is not allowed is pretending:
  reporting cancelled while the span completes and commits behind the observer's back" - exactly
  the delivery-after-cancel defect.

This is a **coverage question for the registry**, not a standard to force onto the code.

## usage-governance-limits: the governing subject is mis-ranked by the map
`usage-limit-governance` (the subject that actually describes the context) scores 499 and ranks
5th, below `quality-gates` (875) and `breach-alerting-and-attribution` (732).

## device-relay and rust-sdk are marked `governance: weak` - both scouts confirmed the warning is correct.

## core-observability-types: real governing subjects only scored `probable`
Scout read every declared join's opening. `agent-memory` (571), `prompt-assembly` (643) and
`agent-runtime-assembly` (423) are all about LLM-agent construction, not a shared type crate ->
treated as unmapped, no techniques applied. `structured-output` (462) is about model-output
schemas and belongs to the judge. `tracing` (519) is genuinely about span trees but its ground
was taken by the 2026-09-04 trace-analytics sweep.

**The real governing subjects are `llm-call-telemetry-model` (472) and
`multi-provider-event-normalization` (570) - both only `probable`.** Worth recording for the map.

## rust-sdk: marked `governance: weak`; the marking is right about the map but its one real join is genuine
`per-provider-usage-extractors` is explicitly a sender-side technique and the SDK conforms to it.
Any OTHER registry guidance aimed at `clients/rust/**` should be treated with suspicion.

## Cross-validation worth noting
Two independent scouts (usage-governance-limits and core-observability-types) found the SAME
scope-ledger divergence from different directions. The second ceded it to the first rather than
duplicating, and contributed two sites the first had not reached. That is the corroboration
signal the critic should weight upward, not a duplicate to void.
