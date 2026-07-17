# Perf+Feature Fix Wave 8 — Feature build-out: the trace tree becomes a debugger

> 2 commits, on branch `vibeman/features-2026-07-17` (off `main`, the merged campaign base).
> Closed trace-trees #1 (product-value Critical) + rust-client-sdk #1 (High). Baseline preserved:
> main workspace 437 passed / 0 failed, 0 warnings (+1 test); client crate 3 passed (+1 test).

## Mental model

The trace tree was the product's flagship differentiator on paper but not in practice: the Rust SDK
could not emit a tree, and even a tree that arrived rendered as anonymous bullets. This wave closes
both ends of that path — **emit** the structure (SDK) and **read** it (render) — so a trace answers
"what went wrong", not just "how much did it cost".

## Commits

| Commit | Finding | Sev | Files |
|---|---|---|---|
| `13e6dd7` | trace-trees #1 | Critical (product value) | `crates/render/src/traces.rs` |
| `557951a` | rust-client-sdk #1 | High | `clients/rust/src/lib.rs` |

## What was fixed

1. **Render: named, debuggable span nodes.** `render_node` read only status/tokens/cost/waterfall, so
   an 11-call agent trace was eleven identical `provider/model` bullets — one ❌ with no reason, no way
   to tell the planner from a tool call, no drill-down handle. `name`, `error`, `id`, and `operation`
   were all already in the payload. Now each bullet is labelled by its call-site `name` (model trails
   as a dim segment, non-`chat` operation appended), carries the short event id as a copy-paste handle
   into the single-event detail view, and — on a failed span — emits an indented sub-bullet with the
   error message. Turns the tree into the `tree → id → payload` debugging path.
2. **SDK: span linkage setters.** `EventBuilder` hardcoded `span_id`/`parent_span_id` to `None` with a
   setter for neither, so every Rust-emitted event was forced to a root and traces rendered flat. Added
   `span_id`/`parent_span_id` (mirroring `trace_id`) + a `build()` finisher. A planner + a tool call
   parented to it, run through the server's own `Trace::from_events`, now nests correctly.

## Verification

| Gate | Result |
|---|---|
| `cargo check --workspace --all-targets` | clean, 0 warnings |
| main workspace tests | 437 passed / 0 failed |
| client crate tests | 3 passed (span setters → nested tree via `Trace::from_events`) |
| render | named steps, model retained, `(embedding)` op shown, `(chat)` suppressed, short id, error sub-bullet — all pinned |

## Patterns established (catalogue, continued)

16. **Read the fields you already ship** — the whole render fix was surfacing payload fields the
    renderer never asked for. When a feature "half works", diff what's serialized against what's shown.
17. **Ship the mechanical blocker first** — the SDK's additive setters (step 1) unblock manual tree
    construction immediately; the ambient-context ergonomics (a tokio dep behind a feature) are a
    separate, deferrable step.

## Deferred (documented — the rest of Wave 8 feature build-out)

- **SDK ambient span context** (rust-client-sdk #1 steps 2-3): `task_local!`/`thread_local!` span
  ownership + RAII `Client::span()` guards so users don't thread ids by hand — adds a `tokio` dep
  behind a default-off `context` feature. Own session.
- **Run-vs-run diff** (benchmark-suites #1, Critical feature): compare mode persists per-case data but
  exposes no two-run diff — an API + render feature.
- **Human relabel of scores** (score-recording #1, High): scores are append-only; add human-label
  fields + a store method (cross-backend schema).
- **Dataset golden-set annotation** (evaluation-datasets #1): item update/delete so captured traces
  become ground-truth (cross-backend).
- **Whole-trace scoring** (trace-trees #2 / score-traces perf): the runner judges only span 0's payload
  — it should see the agent's actual work.
- **Polar backfill + refund reconciliation** (billing #1/#2): the adapter has no sync path; Stripe
  sync ignores refunds.
