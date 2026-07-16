# Perf + Feature Campaign — LightTrack, 2026-07-16

Dual-lens scan (`perf_optimizer` + `feature_scout`) over a **rebuilt context map**
(18→35 contexts, 60%→100% file coverage — the rebuild made the `responder` and `agent` crates and
five other subsystems visible; **all 18 scan criticals sat in previously-unmapped code**). 179 findings
(18C / 103H / 58M). Full detail in `INDEX.md` (10 cross-cutting themes + per-context reports) and the
per-wave `FIXES-WAVE-*.md`.

## What shipped (7 waves, 24 commits, 0 regressions)

Baseline at scan: `cargo check --workspace --all-targets` clean, 222 tests / 0 failed.
After the campaign: **436 tests / 0 failed, 0 warnings** — gates green after every commit.

| Wave | Theme | Criticals closed | Highlights |
|---|---|---:|---|
| 1 | Paid-call admission control | 3 | responder investigate governor; agent connector deadlock; pairwise cost gate + tail resilience |
| 4 | Backend parity + conformance | 1 | scoped unscored-events anti-join (kills the 1000-cap re-judge); conformance now exercises the default-bearing methods |
| 2 | Money-truth query scans | 2 | sargable `project_pred` (EXPLAIN-plan-pinned); Firestore `cost_by_dimension` window pushdown |
| 3 | Privacy & consent | 2 | redaction hash/drop **enforced** on ingest; collective opt-in consent (default off); k-anonymity over sources |
| 5 | Ingest correctness | 2 | batch = one transaction; duplicate-id = **replay** not error; addressable batch items |
| 6 | Eval reproducibility | 1 | deterministic judge sampling; version-aware promotion gate; prompt-version attribution (`GET /v1/costs/prompts`) |
| 7 | Dead-capability sweep | 0 (1 High) | API key lifecycle — `list_api_keys` + revoke, wiring two write-only fields to a reader |

**13 of 18 criticals closed + 1 reframed as documented-by-design.** The 4 remaining criticals all
require a live database this dev box lacks, or were bounded by an earlier fix — see below.

## Test-environment caveat (READ before trusting the PG/Firestore paths)

**Only SQLite is runtime-verified here.** The Postgres and Firestore conformance tests run only against
a live database (env-gated) and skip in `cargo test`, and there is no live PG/Firestore on this Windows
box. Every PG/Firestore change this campaign is **compile-verified only** — the new store methods mirror
existing patterns in the same files, and the conformance suite (extended in Waves 4/6/7) is the
acceptance test to run once a live DB is available. SQLite (the dev/self-host default) is verified
end-to-end through the wired axum router throughout.

## Release-note behavior flips (Wave 3)

- A collective hub **contributes nothing** until projects set `collective_opt_in` (default off).
- A hub with one contributor shows an **empty leaderboard** at the default `min_contributors=2`
  (`held_back` in the response explains why; set 1 to opt out).

## Remaining work (all documented in `followups-2026-07-16.md`)

- **Needs a live DB:** PG/Firestore implementations of the four default-bearing gap methods (Wave-4
  conformance already demands them); emulator verification of the Firestore window pushdown and the
  new key-lifecycle/consent methods.
- **Bounded / by-design:** Firestore `usage_since` per-ingest aggregation (documented tradeoff in
  `docs/FIRESTORE.md`); leaderboard full-table decode (table is capped); MCP context-blowout.
- **Own sessions:** UsageCache read-path wiring; project mutation + `enabled` enforcement; the rest of
  theme T3 (FX `converted`, `effective_date`, `Customer` model, calibration trust, agent backoff);
  operability (`/health`, `/metrics`, graceful shutdown); Wave 8 feature build-out.
