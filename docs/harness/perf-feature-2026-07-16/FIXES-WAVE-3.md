# Perf+Feature Fix Wave 3 — Privacy & consent integrity

> 3 commits. Closed 2 of the 3 wave-3 criticals + 2 Highs; the third critical (a bounded perf issue,
> not a privacy one) deferred with a plan. Baseline preserved: workspace 431 passed / 0 failed
> (+6 tests). Branch `vibeman/perf-feature-2026-07-16` (off `main`, NOT pushed) — 15 commits total.

## Mental model

Every finding here is a **privacy claim the product makes but does not keep**: a redaction setting
displayed but never enforced, a "privacy-safe collective" that ships every project without consent and
publishes single-source rows a filter can isolate. The fixes make the claims true — and where a
behavior flips (opt-in default, contributor floor), the change is disclosed rather than silent.

## Commits

| # | Commit | Finding(s) closed | Sev | Files |
|---|---|---|---|---|
| 1 | `039bdae` | projects-access-control #1 + event-ingestion #2 (scrub bypass) | Critical + High(part) | `api/src/{redact,state,events,events_batch,projects,main,auth,tests_ingest}.rs` |
| 2 | `454131b` | collective-api-rendering #2 (k-anonymity over sources) | High | `api/src/{collective,tests_collective}.rs` |
| 3 | `fef1c68` | collective-api-rendering #1 (consent) | Critical | core + both schemas + 3 backends + api + conformance |

## What was fixed

1. **Per-project redaction policy enforced on ingest.** `Project.redaction` (none|hash|drop) was
   stored, echoed, advertised in the MCP schema, and rendered as a Redaction column — with zero
   consumers and no Hash/Drop implementation: a project showing `drop` stored raw payloads. Now
   `prepare_event` applies the policy (Hash → `{"sha256": …}` per payload; Drop → payloads removed)
   from an AppState cache (startup-warm + create-refresh + lazy backfill; no store read on the hot
   path), then the env PII scrub runs as the floor — which now also covers `error` and `tags`, closing
   the documented-promise bypass. Verified end-to-end through the wired router.
2. **k-anonymity over sources.** Both existing floors counted cases, not contributors; a
   single-source row (however large) published one operator's private evals verbatim, isolable via
   `?provider=`. New `min_contributors` (default 2; 1 = explicit single-tenant opt-out) partitions
   rows before any filter and discloses the suppression as `held_back`.
3. **Collective contribution is now consent-based.** `collective_opt_in` on Project (default OFF),
   column in both schemas + SQLite additive migration + all three backends; `gather_run_stats` walks
   only consenting projects; the digest carries a `projects_included/excluded` consent envelope
   (serde-defaulted, wire-compatible). Conformance pins the flag's cross-backend round-trip.

## Behavior flips (release-note items)

- Existing instances **contribute nothing** to a collective hub until projects are opted in.
- Hubs with a single contributor show an **empty leaderboard** at the default `min_contributors=2`
  (`held_back` discloses why; set 1 to opt out).

## Verification

| Gate | Result |
|---|---|
| `cargo check --workspace --all-targets` | clean |
| workspace tests | 431 passed / 0 failed (+6: policy enforcement e2e, hash/drop/scrub units, k-anon, digest consent) |
| Conformance | `collective_opt_in` round-trip pinned on every backend |

## Deferred (see followups-2026-07-16.md)

- **collective-api-rendering.perf #1 (Critical)** — `/v1/collective/leaderboard` decodes the full
  `collective_entries` table per request (filters post-merge; `idx_collective_model` unused). Deferred:
  it is a *bounded* perf issue (the table is hard-capped at `MAX_ENTRIES`=5000/contributor, and the
  privacy risk that shared its context is now closed by k-anonymity); the proper fix is a filtered
  store method — the backend-parity (Wave 4 follow-up) family, best done with the other store-trait
  additions and conformance coverage.
- The remaining sketch steps for consent UX: `LIGHTTRACK_COLLECTIVE_CONTRIBUTE` master switch +
  `contributable` stamp, `DELETE /v1/collective/contribution` (right-to-withdraw), digest scope
  headline in render. The core consent mechanism (the flag + the filter + the envelope) is shipped;
  these are additive follow-ons.

## Patterns established (catalogue, continued)

9. **A displayed setting must have a consumer** — grep for reads of any policy field the UI renders;
   zero consumers = a false claim, worse than no control. Enforce or remove.
10. **Anonymity floors must count the right population** — a case-count floor does not anonymize
    across contributors; gate on distinct sources, before any filter, and disclose the suppression.
11. **Consent defaults off, and the artifact discloses its scope** — an opt-in flag plus an
    included/excluded envelope turns "I hope nobody runs that command" into a reviewable posture.
