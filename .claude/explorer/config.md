---
product: "LightTrack"
stack: "Rust workspace (axum API, SQLite/Postgres/Firestore stores, runner, MCP) + TS/Python/Rust client SDKs"
vault: ["C:/Users/mkdol/Documents/Obsidian/lighttrack"]
vault_subdir: Explorer
context_map: context-map.json
coverage_context_source: ""
active_runs_ledger: ""
---

# explorer overlay - LightTrack

Project specifics for the lane skill `/explorer` (the engine is linked at `.claude/skills/explorer`).
The vault is the Obsidian vault the other adopted skills (`/architect`, `/perfect`, `/research`)
already share - `Lessons/` and `Patterns/` are common; `Explorer/` is this skill's.

## Context sources
1. `context-map.json` - the area taxonomy (34 contexts, 8 groups; `file_paths`, `entry_points`,
   `cross_refs`, `keywords`).
2. `CLAUDE.md` - repo law: the 300-LOC file rule, wiring-only `main.rs`, backend parity, invariants.
3. `.ai/registry-map.json` - which registry subjects govern each context (read the golden path
   before proposing; log to `.ai/consults.jsonl`).
4. `docs/DECISIONS.md` - the ADR ledger; a decision there is a claim worth checking against code.

## Area menu
Derived from the context map's groups: LLM Observability, Evaluation & Benchmarking, Usage
Governance, Revenue & Profit, Collective Intelligence, Device Relay, Data & Persistence, Platform
Infrastructure.

## Gates
- `cargo build -p <crate>` for every crate touched (never the workspace - see CLAUDE.md).
- `cargo test -p <crate>`; `cargo test -p lighttrack-core --tests` after touching `context-map.json`
  or anything under `.ai/` (the guard tests there pin them).
- `cargo clippy -p <crate> --all-targets -- -D warnings` (blocking in CI) and `rustfmt` per file.
- Store changes: the conformance suite (`cargo test -p lighttrack-store --test sqlite_conformance`)
  plus PG/Firestore parity - a `Store` method one backend implements and another defaults is a bug.

## Repo law
- Read CLAUDE.md first. <= ~300 LOC per file; split by responsibility; tests beside the code.
- `pub(crate)` for cross-module helpers; no `unwrap()` on fallible I/O in library code.
- Wire contracts: read the callers (SDKs under `clients/`, `crates/render`, docs) before changing a
  response field or status code - the render crate reads `/v1/limits/status`, the SDKs do NOT parse
  the ingest response.
- When a context's file list changes, update `context-map.json` and regenerate
  `.ai/registry-map.json` (`node ../ai-registry/scripts/build-registry-map.mjs --project tracklight`).
- Stage explicit paths only; other sessions share the tree.

## Baseline exclusions
None known. Clippy and fmt are clean and blocking.

## Smoke
`sh scripts/smoke.sh http://127.0.0.1:8787` against a locally-run API (`cargo build -p lighttrack-api`
first - `cargo test` does not refresh the exe). Visual surfaces are the render crate's markdown
and the Personas app; the explorer does not verify those - say so.

## Skill improvement log
- 2026-09-01: with an empty coverage ledger the tie-break "smallest file count" selects `cli-tool`
  (1 file). Pick the golden path (event-ingest) or any >=3-file context instead, and say why.
- 2026-09-01: `.ai/consults.jsonl` was untracked, not ignored - added to `.gitignore` this run.
- 2026-09-01: for a `sec` pass, widen along the trust boundary (who owns the flag/policy the area must
  honour) rather than group adjacency - project-management found the `enabled` gap; cost-pricing
  would not have. A parallel session installs skills (`.claude/skills/*`, `.gitignore`,
  `.ai/manifest.yaml` edits) - leave those hunks alone.

