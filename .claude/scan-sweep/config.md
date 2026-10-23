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
- 2026-09-15: this harness exports one `CARGO_TARGET_DIR` across parallel checkouts, which can
  replay a sibling checkout's test binary. Sweep gates must use a checkout-local target directory
  before trusting test names or counts.
- 2026-10-01: `lighttrack-api` is a binary crate. `cargo test -p lighttrack-api --lib` fails
  immediately, and `-- --exact` with only the short test name runs 0 tests and exits 0. Filter
  on the function name with no `--exact`. On Windows PowerShell 5.1, `Get-Content` without
  `-Encoding utf8` corrupts non-ASCII when a test module is extracted into a new file.
- 2026-10-01: a new file named `nested_tests.rs` makes the consult hook route
  `workflow-property-contracts` and `guest-execution-bounding`. Those are filename homonyms.
  Read the openings, log `deviations: 0`, and do not apply their techniques. The hook records
  a miss and does not block the commit.
- 2026-10-01: an MCP `args` name the forwarder does not append (`events_path`,
  `collective_path`, `with_project`) publishes a filter the tool then drops. Put the
  filter on the HTTP row and leave the tool args at the forwarded set. A new test file
  named `jobs_tests.rs` routed `job-coordination` and `engine-host-contract`; same
  homonym rule as above.
- 2026-10-05 (`--challenge`, first challenge run here, 12/12 landed): four operating facts this
  repo forces on a challenge run.
  **(a) `target/` is unusable while this session's own `lt-mcp` MCP server is attached** - cargo
  cannot replace `target/debug/lt-mcp.exe` and the build dies with `os error 5`. Every builder and
  every gate needs a `CARGO_TARGET_DIR` outside the repo (the OS temp dir, never under the tree or
  an overlay).
  **(b) `cargo test --workspace` without `--no-fail-fast` under-reports the total** - it returned
  1913 where the real figure was 1941. Any ledger or baseline comparison must pass the flag or it
  is comparing against a truncated number.
  **(c) `docs/PARITY.md` is NOT a hand-editable shared surface.** It is generated from the
  backends' manifests and guarded by `crates/store/tests/parity_doc.rs`, so a hand-written note in
  it goes red unless the change actually adds a `Surface`. This file listed it as lockable and a
  builder correctly refused; `CHANGELOG.md` is the right home for an operator note. Generated docs
  that ARE safe to append under the lock: `docs/API.md` (via `LIGHTTRACK_WRITE_API_MATRIX=1`),
  `docs/BENCHMARK_FRAMEWORK.md`, `docs/RELAY.md`.
  **(d) `crates/store/tests/soak.rs::store_soak_lane` is a flaky instrument** - it fails its own
  planted-red self-check at roughly 1 in 10 runs *in isolation*, measured 1/13 at an untouched base
  sha against 2/11 on the merged tree. Three builders hit it independently and each re-ran and moved
  on, which is the behaviour to worry about. Brief every builder that it is pre-existing and outside
  their write set, or the run burns an investigation per wave.
- 2026-10-05: `challenge.sharedSurfaces` for this repo is
  `docs/API.md`, `docs/BENCHMARK_FRAMEWORK.md`, `docs/RELAY.md` (append at the END, commit alone
  under the `mkdir` lock). Also serialize these **code** files one-card-per-wave rather than
  locking them: `crates/api/src/main.rs` (5 cards wanted it), `crates/mcp/src/schemas.rs` (3 cards;
  and clippy's `items_after_test_module` forbids appending at the very end, so it is a 3-way merge
  not a union), `crates/core/src/lib.rs`, `crates/contract/src/lib.rs`.
- 2026-10-05: in this repo a new HTTP route now **requires** a `lighttrack_contract::endpoints()`
  row or it answers **500** (the access layer landed this run), and the row needs a `surface` that
  passes a two-way completeness check. Derive the surface from the store methods the route actually
  calls - assert `row.surface == the surface whose methods() contains "<method>"` rather than
  hardcoding it. Also: `tool_contract` ties an MCP `outputSchema` to the endpoint having a
  `render_kind`, so a row without one must NOT get a schema. Two builders' write sets shrank once
  they checked this first.
