# M22 — Contribution as a recorded, scheduled act: contributor-side ledger + serve-loop push

Size L · gate policy · wave D · contexts: model-leaderboard, runner-worker, cli-tool,
store-sqlite-eval, store-postgres · depends on M20 (hub parity), M7 (schedules)

## Problem
Benchmarks are self-running (M7 schedules) but contribution to a collective hub is a manual two-hop
CLI push (`crates/cli/src/collective.rs`: `GET /digest` here → `POST /ingest` there), and the
instance keeps no record of what it sent, when, to which hub, or what the hub acked — the ack is
printed and discarded. `GET /digest` recomputes `projects_included/excluded` per call, so the consent
envelope exists on the wire and nowhere at rest. `DELETE …/contribution` requires the operator to
know each hub URL and key. Nothing triggers a contribution after a run lands.

## Design
1. `core::collective::ContributionRecord { id, hub_url_hash, contributor_id_as_acked: Option<String>, schema_version, generated_at, entries_count, projects_included: u32, projects_excluded: u32, digest_sha256, ack: Value, status: Sent|Rejected|Failed, created_at }`
   — the digest **body is not stored**, only its hash and counts, so the ledger cannot be mined for
   more than the hub already knows.
2. Store: `insert_contribution`, `list_contributions(limit, cursor)`, `latest_contribution(hub_url_hash)`;
   SQLite + PG + Firestore (conformance section; new `Contributions` surface). Table
   `collective_contributions` as a self-contained block at the END of both DDLs; follows the
   never-delete retention rule (ARCHITECTURE §12).
3. API: `POST /v1/collective/contribute { hub, hub_key_ref }` (admin) — builds the digest (existing
   `digest.rs`), hashes it, **skips if unchanged** since the last ledger row for that hub (so a
   no-op push never trips the hub's `min_interval` 429), POSTs through `crate::http` with the key
   resolved from `hub_key_ref` (an env var name or `LIGHTTRACK_COLLECTIVE_HUB_KEY`; never the key
   itself in the body), records the ack. `GET /v1/collective/contributions` (admin).
   `DELETE /v1/collective/contribution?all=1` iterates the ledger's hubs. Routes in `ROUTE_SCOPES`.
4. Runner: `JobKind::Contribute` (M7 enum, additive) dispatched by `serve`; a schedule row of that
   kind (opt-in; `LIGHTTRACK_COLLECTIVE_AUTO_CONTRIBUTE_SECS` creates one at startup if absent) is
   the auto-push — hash-gated and interval-aware.
5. CLI: `lt collective contribute` calls the new endpoint (old direct two-hop kept as `--direct` for
   air-gapped hubs); `lt collective history`; `lt collective withdraw --all`. MCP:
   `get_collective_contributions` (read). Render: contributions table.
6. `docs/BENCHMARK_FRAMEWORK.md` §6 "what left the building" moves from preview to ledger.

## Out of scope
Recommendations (rejected M21). Hub internals (M20, merged).

## Gates
`cargo build/test/clippy` for lighttrack-core, -store, -store-pg, -store-firestore, -api, -runner,
-cli, -mcp, -render; SQLite conformance incl. the new section; a test that an unchanged digest is
skipped (no HTTP call) and a changed one is pushed (against a local axum stub hub).

## Evaluation
Before: 0 contributions recorded anywhere; 0 automated pushes; withdraw needs per-hub knowledge.
After: every push has a ledger row with hash + ack; `withdraw --all` covers 100% of ledgered hubs;
a `Contribute` schedule exists when auto-contribute is on.
