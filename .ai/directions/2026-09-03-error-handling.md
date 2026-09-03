---
subject: software-engineering/error-handling
project: tracklight
raised_by: intake intake-kube-0903 (peer comparison)
source: librarian/sources/2026-09-03-kube-rs.md
stage: the ingest boundary in crates/api and crates/core, where a provider failure becomes an LlmEvent — and crates/responder, which today re-derives the class from prose downstream
size: 3 files / ~150 lines / M
status: accepted
---

## Why the scope implies it

`scope.does` leads with *"ingest LLM telemetry"* (`.ai\manifest.yaml`). The telemetry a client sends most often is a failure, and the single most consequential fact about a failure is its **class** — transient or terminal. tracklight already knows this and has written the rule down. `crates\engine\src\retry.rs:3-4`:

> Classification is by typed `EngineError` variant — never by string-matching provider messages.

One crate over, that rule is broken. `crates\responder\src\classify.rs:38-60` is a second retry-class decision in this workspace and it reads an error **message**:

```rust
let e = error.unwrap_or_default().to_lowercase();
if TRANSIENT_PHRASES.iter().any(|m| e.contains(m)) { return Class::Transient; }
```

Eleven phrases at `:15-27` — `"overloaded"`, `"rate limit"`, `"timed out"`, `"capacity"`, `"connection reset"`, `"econnreset"` — consumed at `crates\responder\src\pipeline.rs:53` to decide whether a spike is worth a paid Claude Code investigation.

**The peer-check seed called this a naive substring matcher. It is not, and the difference is the whole shape of this direction.** The *numeric* half was already hardened, and the comment names the failure it met: a bare `contains("500")` *"fires on `AssertionError: expected 500 got 200` (a real code bug misread as transient, so it is never diagnosed) and even on `processed 5000 rows`"* (`classify.rs:29-35`). So a code now counts only as a whole token, only with an `http`/`status` context word or in leading position (`:46-56`), and both collisions are pinned by tests (`:100`, `:105`). Somebody met the exact failure the corpus predicts and fixed the half that bit them.

**The half that remains is structural, not sloppy.** `spike.error` is an `Option<String>` that crossed a process boundary. The responder is classifying an error **it did not produce**, arriving as prose in a telemetry record, with no variant left to match on. `EngineError` is not in scope there and never was. Deleting the phrase list is not the fix; it is the only thing available at that layer.

The corpus states the correct layer and names the failure mode. `backend-platform/resilience/retry-backoff:47-50`:

> **Classification happens once, at the boundary, against structure.** The layer that still holds the structured response — status codes, error kinds, protocol fields — assigns the class.

And `backend-platform/resilience/error-handling:79-81`:

> Classification must key on **structured fields** — a status code, an error code, a typed variant, a machine-readable field in a response body — never on matching substrings of a human-readable message.

with the reason at `:83-86`: a text classifier is *"a correct program today and a silent misclassifier after any dependency upgrade — and it fails in the worst direction, sliding everything into the catch-all category where retry policy and user copy are at their vaguest."* `error-handling:43-45` names the shape tracklight currently has: consumers that each classify separately *"manufactures three classifiers"*. tracklight has two, and they disagree by construction — `EngineError::is_retryable()` matches three variants (`retry.rs:34-41`); `classify.rs` matches eleven phrases and six codes. Nothing compares them.

**The peer confirms the layer, from a tree that never had the option of prose.** kube's `watcher::Error` states the whole enum's class in the module doc — *"These are all considered retryable from a watcher's point of view"* (`C:/t/kube/kube-runtime/src/watcher.rs:21-48`) — and `finalizer.rs:14-54` warns against `anyhow` by name, because a flattened error has no taxonomy left to read. Its client-side `RetryPolicy` retries a **closed set** of three status codes and nothing else (`kube-client/src/client/retry.rs:50-80`), so a 4xx that is not a rate limit can never be retried by accident. And where kube receives something it cannot type — an object that fails to deserialize — it does not guess from the bytes: `DeserializeGuard<K>` re-parses only `metadata` so the broken object *keeps its identity* and can be logged, evented and reconciled as an error state (`kube-core/src/error_boundary.rs:12-59`). That is the same instinct this proposal applies to a telemetry record: preserve what you know at the boundary rather than reconstruct it downstream.

**And tracklight already does this once, for a neighbouring field.** `crates\core\src\event.rs` canonicalizes a provider string at ingest via `Provider::from_wire()` rather than letting every consumer re-parse it. A retry class is the same kind of fact, minted at the same boundary, for the same reason.

## What the first context contains

**A typed failure class on the event, in `crates\core\src\event.rs`.** A small enum beside the existing `Provider` — `Transient` / `Terminal` / `Unknown` — carried on `LlmEvent`. Three states, not two, and deliberately so: `error-handling`'s sibling rule in `health-checks:50-57` is that collapsing *"could not determine"* into either side fails in opposite directions and both are worse than the truth. An event from a client SDK that did not send the field is `Unknown`, and `Unknown` is not `Terminal`.

**Minted at the boundary, in `crates\api`'s ingest path.** Where a client sends a status code, a provider error code, or a typed field, that is what the class is read from — `retry-backoff:47-50`'s "the layer that still holds the structured response". The three client SDKs (`clients/{rust,python,typescript}`, each with its own `capabilities.test-client-*` command in `.ai\manifest.yaml`) are where the structure is still intact, so the field is theirs to send; the API's job is to accept it, validate it, and default it to `Unknown` when absent rather than inferring it.

**`classify.rs` demoted to a fallback.** `crates\responder\src\pipeline.rs:53` reads the carried class first, and falls through to `classify()` only for `Unknown`. The eleven phrases stay — they are the correct handling for a record whose producer said nothing — but they stop being the primary path, and their *scope* becomes stateable: legacy records and third-party producers. That is exactly `error-handling`'s boundary: a text classifier is not forbidden, it is forbidden **where structure was available and discarded**.

**What it must NOT absorb.** Not `EngineError`'s taxonomy — `crates\engine\src\retry.rs:29-41` is correct as it stands and this study's §1.1 rates it above kube's. Not `crates\responder\src\breaker.rs`'s admission control, which is a separate and correct decision about spend. Not the `Determinism` marker on `GenOutcome`, which records what a provider honoured rather than how it failed. Not a fleet-wide error refactor: `personas`' catch-all `AppError` with retryability reconstructed from strings (`core/src/error_taxonomy.rs:19-56`) is the fleet's real instance of this defect, and it is that project's direction, not this one's.

## The measurable

**Paid investigations spent on provider incidents: currently uncounted, predicted non-zero, target 0.**

Today `pipeline.rs:53`'s transient branch only prints (`:55-59`), so nothing anywhere records how often the classifier was right or wrong. The instrument is a counter on each branch — investigations skipped as transient, investigations spent on records later shown to be transient — and the number that moves is the second. Each false `Class::Code` is a real Claude Code run against a codebase with no bug in it, gated only by the per-project cooldown at `breaker.rs`.

**Second number, and the falsifiable one: classifier disagreement.** Replay a window of stored events through both paths — the carried class and `classify()` on the same message — and count the rows where they differ. That number is the size of the defect, it is computable from data that already exists, and it costs one script. If it is near zero, this direction is not worth doing (see below).

## What would make this wrong

**If the two classifiers already agree.** The disagreement count above is the whole argument, and it should be run *before* the field is added, not after. If replaying a month of stored events shows the phrase list reaching the same verdict as the producer would have, then `classify.rs` is an accurate heuristic over a stable vocabulary and the honest change is a comment at `retry.rs:3-4` acknowledging the exception — not a schema field, three SDK changes and an ingest migration. This is the cheapest possible falsifier and it is available today.

**If clients cannot send the field.** The class must be minted where the structure exists, which is inside the client SDK, in the caller's process. If the dominant ingest path is OTLP (`docs\OTLP.md`) or another wire format tracklight does not control, then there is no place to put the field for most traffic, `Unknown` becomes the overwhelming majority, and the fallback classifier remains the real classifier. The direction then reduces to hardening `classify.rs` — which the numeric half already shows this project can do well — and the schema work is waste. Check the ingest mix first.

**If three states are two states in practice.** `Unknown` earns its place only if a consumer treats it differently from `Terminal`. `pipeline.rs:53`'s two-armed match has no third arm today, and adding a state that immediately collapses into one of the other two at every call site is the *"two-state lie by another door"* that `health-checks:135-144` warns about. If the responder's honest behaviour for `Unknown` is identical to its behaviour for `Terminal`, say so and ship two states.
