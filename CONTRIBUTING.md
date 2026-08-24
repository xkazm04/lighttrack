# Contributing to LightTrack

LightTrack is a self-hosted LLM observability + LLM-as-judge scoring service written in Rust. It
ships as a container, as standalone binaries, and as thin Python / TypeScript / Rust ingest clients.

Bug reports and small, focused PRs are welcome. If you are planning something large, open an issue
first — `docs/ROADMAP.md` and `docs/DECISIONS.md` say where the project is going and which trade-offs
are already settled, and it is no fun to write a patch that argues with a decision made a year ago.

## Orientation

| Read this | For |
| --- | --- |
| `docs/ARCHITECTURE.md` | how the crates fit together |
| `docs/DATA_MODEL.md` | events, traces, scores, benchmarks |
| `docs/BENCHMARK_FRAMEWORK.md` | the judge / scoring side |
| `docs/DECISIONS.md` | why things are the way they are |
| `CLAUDE.md` | the full working agreement (this file is its short form) |
| `context-map.json` | which files belong to which feature |

The workspace is one crate per concern:

| Crate | Binary | What it is |
| --- | --- | --- |
| `crates/api` | `lighttrack-api` | the HTTP server (ingest, query, admin) |
| `crates/runner` | `lt-runner` | the judge / benchmark worker |
| `crates/mcp` | `lt-mcp` | MCP server over the API |
| `crates/cli` | `lt` | operator CLI |
| `crates/agent` | `lt-agent` | device relay agent |
| `crates/responder` | `lt-responder` | responder loop |
| `crates/core` | — | data types |
| `crates/store` | — | the `Store` trait + SQLite backend |
| `crates/store-pg`, `crates/store-firestore` | — | the other two backends |
| `crates/engine` | — | prompts, providers, judge |
| `crates/anon`, `crates/render`, `crates/billing` | — | PII scrub, rendering, cost/margin |

## Build

**Build the crate you changed, not the workspace:**

```bash
cargo build -p lighttrack-api      # or -p lighttrack-runner, -p lighttrack-store, ...
```

This is deliberate. A whole-workspace build is slow and drags in crates you did not touch.

Two things that bite people:

- **`cargo test` does not refresh `target/debug/<bin>`** — it builds a separate test harness. After
  editing a service, run `cargo build -p <crate>` again before launching the binary, or you will run
  a stale one.
- **`lt-runner` loads `.env` from the current directory** — run it from the repo root so
  `GEMINI_API_KEY` / `OPENAI_API_KEY` / `LIGHTTRACK_*` are picked up.

To run the server locally against a throwaway SQLite file:

```bash
cargo build -p lighttrack-api
LIGHTTRACK_DB=data/dev.db LIGHTTRACK_BIND=127.0.0.1:8787 ./target/debug/lighttrack-api
curl -s localhost:8787/health   # -> ok
```

Backend selection is by env: `LIGHTTRACK_DATABASE_URL=postgres://…` → Postgres,
`firestore://<project>` → Firestore, otherwise SQLite at `LIGHTTRACK_DB`.

## Test

The whole suite:

```bash
cargo test --workspace
```

**The three store-conformance suites matter most.** `crates/store/src/conformance.rs` defines one
contract; every backend runs the identical suite, so a codec, upsert, job-claim, or `ORDER BY`
divergence between backends fails the build rather than surfacing in someone's production data:

```bash
# SQLite — in-memory, no infra, always runs
cargo test -p lighttrack-store --test sqlite_conformance

# Postgres — needs a reachable DB; skipped when the env var is unset
LIGHTTRACK_TEST_DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/lighttrack \
  cargo test -p lighttrack-store-pg --test conformance

# Firestore — needs the emulator (`gcloud emulators firestore start`, Java 21+)
FIRESTORE_EMULATOR_HOST=127.0.0.1:8080 LIGHTTRACK_TEST_FIRESTORE=firestore://demo \
  cargo test -p lighttrack-store-firestore --test conformance
```

The Postgres and Firestore suites are env-gated: with no env var they **silently skip**. If you
change anything under `crates/store*`, run at least the Postgres one locally — CI will run all three
anyway, but finding out on the PR is slower than finding out now.

Unit tests live next to the code they cover (`#[cfg(test)] mod tests` in the same module), not in a
parallel `tests/` tree. Integration-level suites (`tests/`) are for the conformance contracts.

**`cargo test --workspace` does not reach the three client SDKs.** It ranges over the `members` list
in the root `Cargo.toml`, and all three clients are outside it — `clients/rust` is its own detached
cargo workspace, and the other two are not cargo at all. Each has its own CI job; run the one you
touched locally:

```bash
cargo test --manifest-path clients/rust/Cargo.toml

cd clients/python && python -m unittest discover tests

cd clients/typescript && npm ci && npm run build && npm test
```

If you change `crates/core` types that a client mirrors, run all three — this is exactly how
`clients/rust` once stopped compiling while every other check stayed green.

## Conventions

These are enforced in review:

- **≤ ~300 LOC per file.** Past that, split by responsibility. Many small single-purpose files beat
  one big one.
- **A binary's `main.rs` is wiring only** — parse args, build the router, dispatch. No business logic;
  it lives in sibling modules. See `crates/runner/src/main.rs` and `crates/api/src/main.rs`.
- **One module per domain, not per layer.** Handlers and commands group by `events` / `scores` /
  `benchmarks` / `jobs` / `prices` / `datasets` / `rubrics`, never one handlers megafile.
- **`lib.rs` is public types + `mod` + `pub use`**, nothing heavy.
- **Store backends**: the `Store` trait stays in `crates/store/src/lib.rs`; a backend implements it by
  delegating each method to a per-domain submodule of free functions (`sqlite/events.rs`,
  `sqlite/scores.rs`, …), with row mappers beside their domain.
- Cross-module helpers are `pub(crate)`; only genuinely public API is `pub`. No `unwrap()` on
  fallible I/O in library code — return `Result`.
- Comments explain **why**, not what, and stay sparse. Match the surrounding style.

### Invariants not to regress

- **Backend parity is a correctness property, not a nicety.** A `Store` method that SQLite implements
  and another backend quietly defaults is how caps and filters silently become advisory. Implement
  it, or return `StoreError::Unsupported` (→ HTTP 501). Never a quiet default.
- Store timestamps are fixed-width `RFC3339(Nanos, Z)` so string range filters and `ORDER BY` are
  correct. Do not introduce a variable-width timestamp.
- The judge/scoring engine is **unbudgeted**; limits apply only to monitored ingest traffic.
- Prices are DB-backed (`model_prices`, seeded once from `config/pricing.json`); after first run the
  DB is the source of truth.
- `lt-mcp` writes **all diagnostics to stderr** — stdout is the JSON-RPC channel. Read tools are
  side-effect-free; write tools stay gated behind `LIGHTTRACK_MCP_ALLOW_WRITES` (default off). The
  MCP server is an HTTP client only, never a direct DB client.

## Pull requests

CI runs on every PR to `main`. **`.github/workflows/ci.yml` is the authority on what blocks** — this
table is a projection of it. It is no longer maintained on trust: `crates/core/tests/gate_table_guard.rs`
reads both files and fails `cargo test --workspace` if a job is missing from the table, a row names a
check that no longer exists, or the Blocking column disagrees with the workflow's `continue-on-error:`.
Add a job, add its row, in the same PR — and spell the check name exactly, because branch protection
is configured from these strings.

| Job (check name) | Blocking |
| --- | --- |
| `sqlite conformance (required)` | yes |
| `postgres conformance (required)` (ephemeral PG service container) | yes |
| `firestore conformance (required)` (gcloud emulator) | yes |
| `cargo test --workspace` | yes |
| `cargo test (rust sdk)` — the detached `clients/rust` project | yes |
| `python suite (python sdk)` — builds the wheel, runs `clients/python/tests` against it | yes |
| `npm test (typescript sdk)` — `npm ci` + `tsc` + the suite + a `dist/` load smoke | yes |
| `cargo clippy -D warnings` | yes |
| `cargo fmt --check` | yes |
| `cargo deny (policy)` — bans, licenses, crate sources | yes |
| `cargo deny (advisories, advisory)` — RUSTSEC feed | **no** — see below |

Clippy and fmt **block**. They were advisory while the tree carried pre-existing debt; that debt was
retired, the tree is stock-rustfmt clean and passes `clippy -D warnings` workspace-wide, and the
gates were promoted so it cannot come back. There is deliberately no `rustfmt.toml`, so plain
`cargo fmt` locally produces exactly what the job checks.

The toolchain is **pinned** in `rust-toolchain.toml` at the repo root, and that is the only place a
Rust version is named — CI installs no toolchain of its own, it just runs cargo through rustup, which
reads the pin. So fmt and clippy judge your commit with the same ruler on every machine and on every
future re-run. (This was not always true: until 2026-08-24 CI floated `stable`, and a new stable went
red on six files nobody had touched. If you ever see that shape of failure again, the pin has been
bypassed — that is the bug, not your diff.)

Bumping Rust is therefore a deliberate one-line change to `rust-toolchain.toml`. Do it in its own PR
and land the fmt/clippy churn the new toolchain wants **in that same PR**, so it never ambushes
unrelated work. Locally, `rustup` picks the pinned toolchain up automatically the first time you run
cargo in this checkout.

`cargo deny` is split into two jobs on purpose, and the axis is **what each half reads**, not how
serious its findings sound:

- **`cargo deny (policy)` blocks.** Bans, licenses and crate sources are a function of `Cargo.lock` —
  the verdict only changes when *you* change dependencies, so a failure is yours and is fixable in
  the PR that caused it.
- **`cargo deny (advisories, advisory)` does not block, permanently.** It reads the RUSTSEC
  database, which moves without us: an entry published overnight against a transitive dependency
  would otherwise wall every unrelated PR until someone lands a bump. This is not a debt schedule
  waiting on a cleanup — no work in this repo retires it. It runs on its own Monday cron (a
  push-only trigger would never see an advisory published during a quiet week) and its output is
  meant to be read, not ignored; an advisory job nobody reads is not a supply-chain check at all.

The three client SDKs each get their own job because each is a **detached project**: `clients/rust`
is its own cargo workspace, and the Python and TypeScript clients are not cargo at all, so
`cargo test --workspace` reaches none of them. They are shipped artifacts, and `clients/rust` has
already once stopped compiling — silently, against a green board — after `lighttrack-core` gained
fields. Anything else this repo ships must get a job of its own too; see the ship-inventory list in
the `ci.yml` header.

Keep PRs scoped to one thing. Fill in the PR template — which crates, which tests you actually ran,
and whether backend parity is affected. "Tests: none" is an acceptable answer for a docs-only change
and an unacceptable one for a store change.

### Secrets

The repository is public. `.env`, `*.local.toml`, and `service-account*.json` are git-ignored —
**never commit an API key.** Before committing, check `git check-ignore .env` and review
`git status`. Stage explicit paths rather than `git add -A`. Secret-scanning push protection is on,
but do not rely on it to catch you.

## Reporting a bug

Open a [bug report](https://github.com/xkazm04/lighttrack/issues/new?template=bug_report.yml). The
form asks which binary, which store backend, and which deployment shape (container / binary / source)
because those three answers determine roughly which half of the codebase the bug is in. Include the
image tag or commit SHA you are running.

Ideas go to a
[feature request](https://github.com/xkazm04/lighttrack/issues/new?template=feature_request.yml).
Plain questions — "does it do X", "how do I point it at Neon" — are fine as a
[blank issue](https://github.com/xkazm04/lighttrack/issues/new); there is no Discussions tab, the
issue tracker is the whole front door.

**Security issues do not go in the issue tracker** — see [SECURITY.md](SECURITY.md).

## License

By contributing you agree that your contributions are licensed under the same terms as the project
(MIT OR Apache-2.0, per `Cargo.toml`).
