# scan-sweep overlay - LightTrack

Project specifics for the lane skill `/scan-sweep` (the engine is linked at
`.claude/skills/scan-sweep` from the ai-registry; declared in `.ai/manifest.yaml` under `skills:`).
The skill runs on its defaults without this file; everything here is what is true about **this**
repo.

| Key | Value |
| --- | --- |
| `contextMap` | `context-map.json` (Personas export, 34 contexts / 8 groups; do not hand-edit) |
| `memoryOutbox` | none - this repo has no `.personas/`; findings go to the vault deck below |
| `backlogDigest` | Obsidian vault `C:/Users/mkdol/Documents/Obsidian/lighttrack` - `Perfect/directions/*` (shipped), `Architect/backlog.md` (queued), plus `docs/ROADMAP.md` "Remaining" |
| `gates` | see below |
| `depth` | defaults (12 loop / 20 `--one`) |
| `neverSweep` | none |

## Gates (per commit, in this order, `&&`-chained, commit as the last link)
- `cargo build -p <crate>` for every crate touched - **never the workspace** (a parallel session
  shares this tree; see CLAUDE.md).
- `cargo test -p <crate>`; when `x.rs` has a sibling test module or `tests/x.rs`, that suite runs.
- `cargo clippy -p <crate> --all-targets -- -D warnings` (blocking in CI); `rustfmt` per file.
- Store changes: `cargo test -p lighttrack-store --test sqlite_conformance` plus PG/Firestore
  parity - a `Store` method one backend implements and another silently defaults is a bug, not a gap.
- Anything under `.ai/` or `context-map.json`: `cargo test -p lighttrack-core --tests` (the guard
  tests pin them).
- Structural ratchet: <= ~300 LOC per file (CLAUDE.md). Check the SITE before choosing the build
  list; a correct fix blocked by the ceiling is backlogged naming that gate.

## Repo law the lenses must not fight
- Judge/scoring engine is unbudgeted; limits apply only to monitored ingest.
- Judge is provider-configurable; prices are DB-backed; timestamps fixed-width RFC3339(Nanos, Z).
- MCP is a thin HTTP client, diagnostics to stderr, writes gated behind
  `LIGHTTRACK_MCP_ALLOW_WRITES`, never mints secrets.
- `pub(crate)` for cross-module helpers; no `unwrap()` on fallible I/O in library code.

## Registry
`.ai/manifest.yaml` declares `knowledge.domains: [software-engineering, llm-observability]` and
`.ai/registry-map.json` carries the context->subject join. Joins below ~400 are frequently homonyms
(`voice-io` on relay-queue/device-agent, `web-scraping` on dataset-management, `i18n` on
rendering-core) - treat them as unmapped until read. Log consults to `.ai/consults.jsonl`, leads to
`.ai/registry-leads.jsonl`.

## Parallel sessions
Stage nothing with `-A`/`.`/`-u`; commit with `git commit -m "..." -- <paths>` and no separate
`git add`. Check `git status --porcelain -- <context paths>` before reading and before each commit.

## Ad-hoc lenses run here
- `moonshot-architect` (2026-09-01) - operator-named, absent from `references/lenses.md`; run as a
  cross-group read-only scout fan-out under `--develop`, L+ only, no builds. Recorded in the
  snapshot as `lens_keys: []` with the ad-hoc key in `note`, so the coverage ledger does not count
  it as a registered lens pass. Deck: vault `ScanSweep/moonshot-2026-09-01.md` (30 items from 41
  cards; `ScanSweep/` is this skill's folder in the vault - `Perfect/` and `Architect/` are other
  skills' and are not written here).

## Skill improvement log
- 2026-09-01: adopted; first run was the moonshot round above. No stabilize round yet - the
  coverage ledger is 0/34; `coverage.mjs --next` says `alert-delivery`.
