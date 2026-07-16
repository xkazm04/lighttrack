# Perf+Feature Fix Wave 5 — Ingest correctness (the front door)

> 3 commits. Closed both wave-5 criticals + 1 High + 2 Mediums. Baseline preserved: workspace
> 433 passed / 0 failed (+2 tests). Branch `vibeman/perf-feature-2026-07-16` (off `main`, NOT
> pushed) — 19 commits total across Waves 1/4/2/3/5.

## Mental model

Ingest is the product's front door: every customer LLM call lands here, and every SDK author codes
against its semantics. The wave makes the front door **safe to retry** (replay, not error), **cheap
to batch** (one transaction, not 500), and **programmatically usable** (addressable, coded results).

## Commits

| # | Commit | Finding(s) closed | Sev | Files |
|---|---|---|---|---|
| 1 | `d445bc4` | ingest-hardening perf #1 (batch = 500 autocommits + 500 rule reloads) | Critical | `store/src/sqlite/{mod,events}.rs` |
| 2 | `1bec836` | ingest-hardening feature #1 (no idempotency) + #3 (unaddressable items) + perf #2 (double clone) | Critical + 2×Med/High | `api/src/{events,events_batch,tests_ingest}.rs` |
| 3 | `221b847` | ingest-hardening perf #3 (per-rejection full-map prune) | Medium | `api/src/rejections.rs` |

## What was fixed

1. **Batch ingest is one transaction + one rule read per project.** Each item was its own autocommit
   (one fsync per event — a 500-item batch held the *global* connection lock ~0.5–2.5s, stalling every
   tenant, making the "efficient" path worse than 500 interleaved single posts) and re-queried the
   same project's limit rules per item. Now: one `unchecked_transaction` (connection mutex already
   held), rules hoisted per distinct project via `insert_checked_with_rules`. Per-item failures don't
   poison the transaction; the existing cap-honesty and siblings-stay-committed tests pin the
   unchanged admission semantics.
2. **A retried write is a replay, not an error.** A PK collision now compares the stored row's
   identity scalars (project, ts, provider, model, token counts — deliberately not cost or payloads,
   which legitimately drift between attempts): same logical event ⇒ 200/`accepted` with
   `duplicate: true`, returning the original outcome, nothing double-counted; different payload under
   the same id ⇒ a true 409. Retry-safety requires a client-generated id + ts — the shipped SDKs set
   both at construction, so their resends are recognized. This is the durable PK-based backstop; a
   request-envelope `Idempotency-Key` fast path remains open follow-up.
3. **Batch items are addressable and machine-readable.** Every variant carries `index`; `Invalid`
   carries the client id when present and a stable `code` (`bad_request|conflict|rate_limited|internal`
   — the taxonomy the single path already used); raw store errors log server-side instead of leaking
   onto the wire. Both per-batch deep-clones killed (events moved, vector round-tripped through the
   closure) — an 8 MiB batch no longer sits in memory ~3×.
4. **Rejection-ledger prune amortized** off the hot path (60s gate vs. per-rejection full-map retain
   under the shared mutex); `snapshot` still prunes eagerly so staleness can never surface.

## Verification

| Gate | Result |
|---|---|
| `cargo check --workspace --all-targets` | clean, 0 warnings |
| workspace tests | 433 passed / 0 failed |
| New pins | replay acknowledged w/ original outcome + no double-count (single + whole-batch resend, per-item `index`/`duplicate`); different-payload 409; ts-less-body 409 documented |

## Patterns established (catalogue, continued)

12. **Duplicate-key = retry until proven otherwise** — compare identity scalars against the stored
    row; acknowledge replays with the original outcome, refuse true conflicts. Requires client-supplied
    id + ts; exclude fields that legitimately drift between attempts (prices, redaction output).
13. **Batch loops: hoist invariants, one transaction, move don't clone** — per-item config reloads and
    autocommits are invisible at the call site and multiply under a held global lock.

## What remains (wave-5 tail → followups)

- ingest-hardening feature #2 (High): generalize the rejection ledger into a **drop ledger**
  (`DropReason` enum + `GET /v1/ingest/health`) — the "why are my events missing?" answer. A
  self-contained DX feature, deferred to keep this wave one mental model.
- `Idempotency-Key` request-envelope fast path (feature #1 step 2) + SDK retry loop (step 4).
