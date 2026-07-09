# CLAUDE.md — working agreement for LightTrack

LightTrack is a self-hosted LLM observability + LLM-as-judge scoring/benchmark tool (a leaner,
data-open alternative to Langfuse). Rust workspace. Design docs live in `docs/`
(`ARCHITECTURE.md`, `BENCHMARK_FRAMEWORK.md`, `DECISIONS.md`, `ROADMAP.md`, `PACKAGING.md`) — read
those for *what* and *why*; this file is *how we write the code*.

## Code structure & composability (enforce in every change)
- **≤ ~300 LOC per file.** If a file grows past it, split by responsibility. Prefer many small,
  single-purpose files over one large one.
- **Binaries' `main.rs` is wiring only**: parse args / build router / dispatch. No business logic.
  All logic lives in sibling modules. (See `crates/runner/src/main.rs` and `crates/api/src/main.rs`.)
- **One module per domain or concern.** Group handlers/commands by domain (events, scores,
  benchmarks, jobs, prices, datasets, rubrics, …), not by layer-mixing megafiles.
- **Shared types** (state, error, DTOs, outcomes) go in small dedicated modules; a crate's `lib.rs`
  holds the public types + `mod` declarations + `pub use` re-exports, nothing heavy.
- **Library crates** (`core`, `engine`, `anon`): split by concern. `core` = one file per data type.
  `engine` = `prompts` / `claude` / `providers` / `judge` (the reference example of the pattern).
- **Store backends**: the `Store` trait stays in `crates/store/src/lib.rs`. A backend implements it by
  delegating each method to a per-domain submodule of free functions over a locked `&Connection`
  (e.g. `sqlite/events.rs`, `sqlite/scores.rs`); row mappers live beside their domain.
- **HTTP clients**: a thin `http` module with `get`/`post`; callers pass typed bodies.
- **Tests** live next to the code (`#[cfg(test)] mod tests`) in the module they cover.

## Rust idioms
- Match the surrounding code's naming, comment density, and import style. Comments explain *why*,
  not *what*; keep them sparse.
- Cross-module helpers are `pub(crate)`; only genuinely public API is `pub` (and re-exported from
  `lib.rs`). No `unwrap()` on fallible I/O in library code — return `Result`.
- Keep functions focused; if a function needs many `#[allow(clippy::too_many_arguments)]`, consider a
  small params struct.

## Build / test workflow (Windows, this repo)
- **Always build the specific crate** you changed: `cargo build -p <crate>`. A parallel session is
  active (see below) — building the whole workspace can pull in their in-progress code.
- **`cargo test` does NOT refresh the runnable `target/debug/<bin>.exe`** — it builds a separate test
  harness. After editing a service, run `cargo build -p <crate>` before launching the exe, or you'll
  run a stale binary.
- `lt-runner` loads `.env` via dotenvy from the **current directory** — run it from the repo root so
  `GEMINI_API_KEY` / `OPENAI_API_KEY` / `LIGHTTRACK_*` are found.
- On Windows, `claude` is an npm install (only `.cmd`/`.ps1` shims on PATH, which a child process
  can't invoke with quote-heavy args). The runner auto-resolves the real `claude.exe` under
  `%APPDATA%\npm\node_modules\@anthropic-ai\claude-code\bin\`.
- Smoke-test changes against a locally-run API before committing; keep the test suite green.

## Secrets & safety
- `.env` (and `*.local.toml`, `service-account*.json`) are git-ignored. **Never commit API keys.**
  Before committing, `git check-ignore .env` and review `git status` — stage explicit paths, not `-A`,
  when other sessions have untracked work.
- The remote (`github.com/xkazm04/lighttrack`) is **public**.

## Parallel-session coordination
- A second session works in this **same working tree** on Phase 5 (packaging) and the **Postgres**
  backend (`crates/store-pg`, `LIGHTTRACK_DATABASE_URL`, API on `Arc<dyn Store>`).
- **Leave Postgres-adjacent code to them**: `crates/store-pg/**` and the store-selection block in
  `crates/api/src/main.rs`. Don't refactor those without coordinating.
- Their commits land in shared local history. Before pushing: `git fetch origin` then
  `git rev-list --left-right --count origin/main...HEAD`; push fast-forwards, rebase only if diverged.
- Commit only your own files; leave their untracked work (`store-pg/`, `Cargo.lock` churn) alone.

## Key invariants (don't regress)
- The judge/scoring engine is **unbudgeted**; limits apply only to monitored ingest traffic.
- Judge is **provider-configurable** (`judge_model = "[provider/]model"`); judging is a structured
  generation parsed from the model's JSON text. Prefer a judge family different from the generator
  (self-preference bias).
- Prices are **DB-backed** (`model_prices`, seeded from `config/pricing.json`); generation cost for
  providers that return no `$` is priced from the book by tokens.
- Store timestamps are fixed-width `RFC3339(Nanos, Z)` so string range filters / `ORDER BY` are correct.
- MCP server (`lt-mcp`): all diagnostics to **stderr** — stdout is the JSON-RPC protocol channel.
  Read tools are side-effect-free (annotated `readOnlyHint`); **write tools stay gated behind
  `LIGHTTRACK_MCP_ALLOW_WRITES` (default off)** on top of the API's admin-key checks. Don't expose
  secret-minting (API keys) over MCP — it would leak into agent context. The server is a thin HTTP
  client only: never give it direct DB access.
