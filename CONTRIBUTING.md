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

### Run the gates before you push

```bash
git config core.hooksPath .githooks   # once per clone
sh scripts/gates.sh                   # or --fast for fmt + clippy + the workspace suite
```

`scripts/gates.sh` runs **the same commands CI blocks on** — not a hand-copied approximation:
`crates/core/tests/manifest_guard.rs` fails the build if any capability in the manifest's
`controls.ciHardPass` is missing from the script or spelled differently there. Add a blocking gate
and the local rung is red until it runs it too.

`.githooks/pre-push` runs it for you. Push, not commit: a work-in-progress commit that does not
compile is a legitimate thing to make, and a rung that forbids it gets bypassed within a day —
taking the pre-commit secret scan down with it. `LIGHTTRACK_SKIP_GATES=1 git push` bypasses it
deliberately (prefer that to `--no-verify`, which also silences the secret scan). A gate whose engine
is not installed locally announces the skip and does not fail your run; CI installs every engine and
blocks unconditionally, which is where the guarantee lives. The point of the local rung is that the
remote run becomes a **confirmation** rather than a discovery.

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
| `chart policy (required)` — `deploy/helm/lighttrack` against its deployment policies + must-fail fixtures | yes |
| `cargo test (rust sdk)` — the detached `clients/rust` project | yes |
| `python suite (python sdk)` — builds the wheel, runs `clients/python/tests` against it | yes |
| `npm test (typescript sdk)` — `npm ci` + `tsc` + the suite + a `dist/` load smoke | yes |
| `cargo clippy -D warnings` | yes |
| `cargo fmt --check` | yes |
| `cargo deny (policy)` — bans, licenses, crate sources | yes |
| `cargo deny (advisories, advisory)` — RUSTSEC feed | **no** — see below |
| `gitleaks (secrets)` — full-history secret scan, pinned engine | yes |
| `gitleaks (latest rules, advisory)` — Monday cron, newest upstream rules | **no** — see below |

### The documentation recovery lane

Per-change enforcement always leaks — dismissals accumulate, a surface gets imported from before the
discipline existed, a pass runs out of budget — so the repair for accumulated drift is a **batch
pass**: read the current source truth, rewrite what drifted. What keeps that from being an
open-ended rewrite-everything campaign is a **catch-up marker** per surface family, recording what
the last pass did and against what:

| family | marker | what it covers |
| --- | --- | --- |
| reference docs | `docs/catchup-marker.json` | `docs/**` — design and operations reference |
| agent contract | `.ai/catchup-marker.json` | `.ai/`, `CLAUDE.md`, `CONTRIBUTING.md`, `README.md`, `SECURITY.md`, `context-map.json` |
| client SDK docs | `clients/catchup-marker.json` | the READMEs that ship inside the published packages |

Three markers, not one: those families drift at different rates and are repaired by different
passes, and a shared marker would force the narrowest pass to lie about the widest surface.

`sh scripts/docs-catchup.sh [family]` turns "should someone do a docs pass?" into a computable
question — how many commits since the anchor, how many of this family's files changed in them, and
how long is the list the last pass did not reach. It **refuses loudly** on a missing or unparseable
marker rather than defaulting to a full rewrite or a silent no-op.

Two rules, both enforced by `crates/core/tests/catchup_marker_guard.rs` on every
`cargo test --workspace`:

- **Every file under a family's surfaces is in exactly one of `covered` / `skipped`.** So a doc added
  tomorrow cannot join no list; the build says so in the same change that added it. That is what
  makes "full pass" a predicate rather than a claim — a later reader can tell "this was current as of
  the anchor" from "this was never in scope".
- **A marker records what was done, never what is hoped.** Predictions ("this cannot drift again")
  are rejected mechanically. The technique this follows exists because of a marker that ended *"the
  per-session hook now prevents this kind of drift; bulk rewrites should not be needed again"* —
  written on the day the never-fired hook landed. The next operator trusted it and scoped narrow; the
  truth was zero enforcement and fifteen months of drift. When the mechanisms around a marker change,
  it gains a dated note *that* they changed, never an assertion that they work.

A pass's final act is updating its marker, **in the same commit as the repairs** — a repair committed
without its marker update recreates exactly the ambiguity the marker exists to remove.

One lane is deliberately **not** in that table and not in `ci.yml`: the store soak lane
(`.github/workflows/soak.yml`, nightly). A long lane is a certification, not a gate — it judges
behaviour over time, which is not a property of any single change — so it runs on its own clock and
never walls a merge. Its criteria are committed at `docs/harness/soak-criteria.json` and its
contract is `docs/harness/soak-lane.md`. The same harness runs briefly inside
`cargo test --workspace`, where it asserts only that the lane is alive and still fires on its planted
defect; the timing bounds are the nightly's verdict.

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

`gitleaks` is split on the same axis, for the same reason:

- **`gitleaks (secrets)` blocks**, on a **pinned** engine (the version is in the job's `env:`). It
  scans the whole history, not just your diff — 241 commits in about seven seconds, so there is no
  latency argument for scanning less. Pinned because a scanner's verdict is a function of *this
  repository × its rule set*, and an unpinned engine would make a blocking gate non-deterministic
  across time, exactly the property that disqualified the floating toolchain above.
- **`gitleaks (latest rules, advisory)` does not block.** It runs the *newest* upstream rules over
  the whole history on the Monday cron, because rules improve after commits land and a pattern added
  today should find a token pushed last spring — an input that moves without us, so it reports
  rather than walls. Triage its findings into `.gitleaksignore` as **fingerprints with a reason**
  (never a directory exemption), or by rotating; a run of clean Mondays is the cue to bump the pin.

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
**never commit an API key.** Stage explicit paths rather than `git add -A`.

Two rungs now scan for credentials, and only one of them is a gate:

```bash
git config core.hooksPath .githooks   # once per clone — git never runs a cloned repo's hooks by default
```

- **`.githooks/pre-commit`** runs `gitleaks` over the **staged diff** — the exact bytes about to
  become history, not the working tree (under partial staging those differ). If gitleaks is not
  installed it **announces the skip and lets the commit through**: failing a fresh clone on a missing
  tool teaches `--no-verify`, and that habit persists into the commit that mattered.
- **`gitleaks (secrets)` in CI** scans the full history on every PR and push and **blocks**. That is
  the rung a leaked token cannot pass. The local hook is the fast answer, not the guarantee.

If the scanner flags something of yours: if it *is* a secret, unstage it — it has not entered shared
history yet, so the fix is still deleting a line rather than rotating a key. If it is not, make the
value obviously fake. Only if it genuinely must stay does it get a **fingerprint** in
`.gitleaksignore`, with the reason written next to it — never a path or directory exemption, which
is a permanent blind spot exactly where fixtures and configs collect. A real example value is banned
even in a test.

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
