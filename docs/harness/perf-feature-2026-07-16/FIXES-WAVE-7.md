# Perf+Feature Fix Wave 7 — Dead-capability sweep (theme T3): API key lifecycle

> 1 commit. Closed projects-access-control #2 (High). Baseline preserved: workspace 436 passed /
> 0 failed, 0 warnings (+1 test, +conformance). Branch `vibeman/perf-feature-2026-07-16` — 24 commits.

## Mental model

Theme T3 is fields the code *writes but never reads* — a promised control with no consumer. This wave
takes the two on the access-control trust boundary (`last_used_at`, `revoked`) and gives them a reader.

## Commit

| Commit | Finding | Sev | Files |
|---|---|---|---|
| `87784d5` | projects-access-control #2 | High | store trait + 3 backends + conformance + `api/src/projects.rs`, `main.rs`, `tests_ingest.rs` |

## What was fixed

`last_used_at` was written on every authenticated request and `revoked` was honored on the hot path,
but nothing could read last-use back or set revoked — a write-only audit column and an
enforced-but-unreachable flag, so leak response was a hand-run `UPDATE api_keys`. Added `list_api_keys`
(default `Ok(vec![])`, matching the `get_limit_rule` precedent) and `set_api_key_revoked` (default =
clear unimplemented error) to the trait and all three backends; `GET /v1/projects/:id/keys` (projected
`KeyInfo`, never `key_hash`) and `DELETE /v1/projects/:id/keys/:kid` (soft revoke, path-scoped, 404 on
unknown). Revocation is immediate — auth reads the store per request.

## Verification

| Gate | Result |
|---|---|
| `cargo check --workspace --all-targets` | clean, 0 warnings |
| workspace tests | 436 passed / 0 failed |
| Conformance | list + revoke round-trip pinned on every backend |
| e2e | project key can't list (admin-gated), no key_hash leak, revoked key flips OK→401 next call |

## Deferred (documented — the rest of theme T3)

Deliberately scoped to one clean, hot-path-free closure before shipping the campaign PR. The remaining
dead-capability findings each carry a real constraint and belong in follow-up sessions:

- **projects-access-control #3 (Medium) — `enabled`/name/redaction unsettable.** `PATCH /v1/projects/:id`
  + `update_project` are additive, but *enforcing* `enabled` on ingest is a hot-path read that must go
  through (and extend) the Wave-3 redaction-policy cache — adding a settable-but-unenforced `enabled`
  would just recreate the anti-pattern this campaign fights, so it's enforce-or-nothing, deferred.
- **FX `converted` lossy** (margin-sim #2) — persist original minor amount + rate; a `RevenueEvent`
  schema change across all three backends, needs live-DB verify.
- **`effective_date` never read / price book overwrites** (model-pricing #H2) — effective-dated pricing
  is a schema + query change; the price book keying on `(provider, model)` makes history structural.
- **`Customer`/`BillingProduct` modeled, constructed nowhere** (revenue-margin #1) — a "Phase 2 sync"
  product decision, not a bug.
- **calibration `bias`/`trusted` measured, unconsumed** (judge-engine) — wiring the trust signal into
  the gate overlaps the calibration-watch findings; own session.
- **agent `retry_after_secs` unpopulatable** (device-agent #1) — the full fix is a `paused_until`
  rate-limit loop in the agent, a behavioral change worth its own session.
