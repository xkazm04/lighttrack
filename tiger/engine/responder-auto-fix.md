---
note_type: engine-call-site
call_site: responder-auto-fix
task: Apply a minimal repository fix from a prior diagnosis under explicit opt-in and branch isolation
use_case: UC1
modality: text
entry: crates/responder/src/act.rs:85
wrapper: lighttrack_engine::invocation::run in Edit mode
prompt_builder: crates/responder/src/act.rs:163
output_contract: none; success is inspected git changes plus optional test outcome
providers: [anthropic-claude-cli]
model: responder default sonnet, configurable in map
grounding: "3/4 in-direction ; out-direction open: branch/diff/test/cost survive, original alert evidence and full model provenance do not"
dials: { wrapping: 8/10, observability: 6/10, caching: 2/10 }
fingerprint: cc6baea5fb22a11b9ab8f8846bf7f306c969cd1c9d1bd0b5a272947bfd9fd6e8
status: discovered
characters: ["[[tomas-novak]]", "[[priya-raman]]", "[[daniel-brooks]]"]
last_reviewed: 2026-09-15 (init; no session)
tags: [engine, llm-call-site]
---
# Engine call site - responder-auto-fix

## What the model is asked to do

Apply a minimal fix only when confident, without unrelated refactoring, test weakening, or git
commands. The call runs only after project auto_fix opt-in, a clean-tree check, a new lt-fix branch,
and circuit-breaker admission. The host commits, runs the configured test, and restores the original
branch only when the commit succeeded.

## Grounding audit (Lens B) - grounding 3/4

1. Current mapped project/repository - working directory plus project id at crates/responder/src/act.rs:163-175.
2. Prior read-only diagnosis - inserted at :167-168.
3. Configured verify command - inserted at :164,173.
4. Original alert/event evidence - not directly included; only whatever the diagnosis retained.

Prompt evidence: make a minimal fix only if confident; otherwise make no changes and explain why.
No edit run was invoked at init. A live fixture must use a disposable worktree, never this checkout.

Out direction records skipped reason, branch, whether changes were applied, optional test result,
notes, and cost. ActOutcome does not carry the edit model, tokens, latency, prompt identity, diff
digest, or explicit rollback artifact.

## Lens A - Engine Quality dials

- Wrapping 8/10: explicit Edit posture/acceptEdits, clean-tree and branch containment,
  default-off project consent, circuit breaker, budget/timeout, post-run change detection, host
  commit/test, and safe handling of commit failure. Model output itself is unstructured.
- Observability 6/10: branch/applied/test/notes/cost are visible and failed commits stay on the fix
  branch. Model/token/latency/prompt/diff identity are not all in the outcome.
- Caching 2/10: circuit-breaker timing prevents repeated mutation; caching a generative fix would be
  unsafe without diagnosis/revision keys and no such cache exists.

## Lens C - model fit

Patch generation is unbounded and high consequence. Sonnet@medium is the predicted minimum, but no
configuration is sufficient unless the same diagnosis yields a minimal correct diff and passing
independent tests. A larger model cannot replace consent, containment, or verification.

## Findings

None. Any change to this path would require an approved proposal; init builds nothing.

## Strengths (do not refactor away)

Default-off consent, clean-tree refusal, dedicated branch, host-owned commit/test, circuit breaker,
minimal-change instruction, and refusal to switch away after a failed commit.

Fingerprint input: crates/responder/src/act.rs:163-175.
