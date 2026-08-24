# architect overlay - LightTrack

Project specifics for the lane skill `/architect` (the generic engine lives in the ai-registry skills
lane and is linked at `.claude/skills/architect`). The scan runs without this file, on the lane's
defaults; everything here is what is true about **this** repo.

## Repo

LightTrack - a Rust workspace: self-hosted LLM observability + LLM-as-judge scoring/benchmark service.
Root: `C:\Users\mkdol\dolla\LightTrack`. Remote `github.com/xkazm04/lighttrack` is **public**.

## Vault

```bash
VAULT="C:/Users/mkdol/Documents/Obsidian/lighttrack"
```

`Architect/` (scans, decisions, backlog, strong-patterns, weak-patterns, coverage) and
`Patterns/architect-preferences.md` are this skill's. `Lessons/` and `Perfect/` are shared with the
other adopted skills - do not recreate or disturb them.

## Reference files

- `context-map.json` (repo root) - area taxonomy and target file lists. A **Personas app export**
  (`generator: personas-context-scan`, `version: 2`); today 8 groups / 34 contexts, last scanned
  2026-08-03. Do not hand-edit it; ask for a rescan. If it is missing, stop.
- `CLAUDE.md` - the working agreement (structure rules, Rust idioms, build workflow, invariants).
  **Read in full**, and treat it as the authority when it disagrees with anything here.
- `docs/ARCHITECTURE.md`, `docs/DECISIONS.md` - the *what* and *why*. Heavily consulted in scan mode;
  a finding that contradicts a recorded decision re-opens the decision rather than silently
  overriding it.
- `docs/BENCHMARK_FRAMEWORK.md`, `docs/DATA_MODEL.md`, `docs/PRICING.md`, `docs/PACKAGING.md`,
  `docs/CI_GATE.md`, `docs/ROADMAP.md` - as relevant to the theme.
- `deny.toml` - the dependency policy (`cargo deny check all --locked`, advisory in CI).

## Themes (Q2a menu)

```
  2. store-backend-parity     (Store trait x SQLite/Postgres/Firestore drift)
  3. error-handling           (LtError/anyhow boundaries, HTTP error mapping)
  4. api-surface              (handler shape, auth guards, DTO consistency)
  5. data-modeling            (core types, timestamps, ids, serde contracts)
  6. testing-strategy         (conformance suite reach, test placement, gaps)
  7. async-patterns           (tokio usage, blocking-in-async, spawn discipline)
  8. provider-boundary        (engine providers, judge, retry/pool, HTTP clients)
  9. config-and-env           (env vars, defaults, feature flags, dotenvy)
```

Theme-specific angle swaps (replacing the lane's defaults where listed):

- `store-backend-parity` -> usage map, type/contract, test coverage, plus **"conformance-suite reach
  vs trait-method count"**.
- `error-handling` -> usage map, type/contract, failure mode, test coverage.
- `api-surface` -> usage map, type/contract, failure mode, plus "auth/validation at the boundary".
- `data-modeling` -> usage map, type/contract, plus "schema-vs-type drift" and "timestamp/id discipline".
- `testing-strategy` -> test coverage (deeply), plus "fixture duplication" and "harness reach".
- `async-patterns` -> usage map, type/contract, failure mode, performance surface.
- `provider-boundary` -> usage map, type/contract, failure mode, plus "retry/timeout consistency".
- `config-and-env` -> usage map, type/contract, plus "default drift across binaries" and
  "documented-vs-actual env vars".

Rust-shaped readings of the generic angles: type/contract means trait boundaries, leaky abstractions
and serde/DTO drift; failure mode means `Result`/error consistency, recovery, observability and
`unwrap()` in library code; performance surface means blocking-in-async, lock scope, N+1 queries and
allocation churn; test coverage means unit vs conformance vs API tests.

## Areas (Q2b menu)

The 8 groups in `context-map.json`: **LLM Observability** (traces, privacy), **Evaluation &
Benchmarking** (judge scoring, datasets), **Usage Governance**, **Revenue & Profit** (cost, margin),
**Collective Intelligence**, **Device Relay**, **Data & Persistence**, **Platform Infrastructure**
(MCP, CLI, SDK, responder). Free text resolves against context names and `file_paths`.

## Gates and baselines

Capture baselines before executing and measure **delta, not absolute**:

```bash
cargo build -p <touched crates>                    # must succeed
cargo test -p <touched crates>                     # baseline pass/fail
cargo clippy -p <touched crates> 2>&1 | tail -5    # baseline warning count
```

Per rollout step: no new clippy warnings beyond +5, tests at the baseline rate, build green. CI's
hard-pass set is `test`, `conformance`, `lint` (`cargo clippy --workspace --all-targets -- -D
warnings`), `format-check`; `cargo deny` is deliberately advisory.

- **Always build the specific crate** (`cargo build -p <crate>`) - a parallel session shares this tree
  and a workspace build pulls in their in-progress code.
- **`cargo test` does NOT refresh `target/debug/<bin>.exe`.** Rebuild before smoke-testing a binary.
- Smoke-test against a locally-run API when the change affects a service surface
  (`sh scripts/smoke.sh http://127.0.0.1:8787`).

## Coordination - parallel sessions

`CLAUDE.md` § Parallel-session coordination is the authority and it moves; re-read it every run.
As of 2026-08-03 `crates/store-pg/**` is **no longer reserved** - the Postgres backend landed and
carries production traffic, so it is ordinary in-scope code. The store-selection block in
`crates/api/src/main.rs` is still delicate: it decides which backend a deployment gets, so change it
deliberately, never as a drive-by.

Multiple sessions share this working tree: inspect `git status --short`, classify every dirty path as
theirs / yours / in-your-touch-zone, and stage explicit paths only. Forbidden at every phase:
`git add -A` / `.` / `-u`, `git stash`, `git reset --hard|--merge`, `git restore` / `git checkout --`
on any path, `git clean`.

## Codification vehicles (Phase 7B)

```
  1. docs-claude  - append a convention to CLAUDE.md (surfaces in every session)
  2. docs-arch    - append a section to docs/ARCHITECTURE.md or docs/DECISIONS.md
  3. test-guard   - a Rust test that walks the tree / asserts the invariant (fails on drift)
  4. lint-gate    - clippy/rustfmt configuration or a CI grep gate in .github/workflows/ci.yml
```

Cross-file invariants (file LOC caps, "no `unwrap()` in lib code", trait-method-vs-conformance
parity) suit a `test-guard` that walks the tree with `std::fs`. Place test-guards under the most
relevant crate's tests with a failure message pointing at the strong-patterns entry, and confirm they
pass on current code before commit. **Keep new CI lint-gates advisory unless the user says
ship-blocker** - that matches this repo's existing `ci.yml` philosophy.

## Project invariants - non-negotiable in any commit

From `CLAUDE.md` § Key invariants:

- The judge/scoring engine is **unbudgeted**; limits apply only to monitored ingest traffic.
- Judge is **provider-configurable** (`judge_model = "[provider/]model"`); prefer a judge family
  different from the generator (self-preference bias).
- Prices are **DB-backed** (`model_prices`, seeded from `config/pricing.json`).
- Store timestamps are fixed-width `RFC3339(Nanos, Z)` so string range filters / `ORDER BY` are correct.
- `lt-mcp`: all diagnostics to **stderr** (stdout is the JSON-RPC channel); read tools side-effect-free
  and annotated `readOnlyHint`; write tools stay gated; no secret-minting over MCP; MCP stays a thin
  HTTP client.
- **Backend parity is a correctness property.** A `Store` method that SQLite implements and another
  backend silently defaults is how caps and filters become advisory. Implement it, or return
  `StoreError::Unsupported` (-> 501); never quietly default.
- <= ~300 LOC per file; `main.rs` is wiring only; no `unwrap()` on fallible I/O in library code.
- **Never commit `.env` or keys.** `git check-ignore .env` before committing; the remote is public.

## Commit shape

`architect: <step title>`, one atomic commit per rollout step, staging only that step's paths, with
the ADR wikilink in the body and the SHA recorded back into the ADR. Default is to commit on the
current branch (this tree hosts concurrent sessions); offer an `architect/<slug>` branch only for a
risky migration meant to be reviewed as a unit, and never push toward it.

## Where a durable fact goes

A structural fact future sessions need -> `docs/ARCHITECTURE.md`, tagged with the run date. A working
rule every session must know -> `CLAUDE.md`.
