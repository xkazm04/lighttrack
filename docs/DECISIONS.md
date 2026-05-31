# LightTrack — Decisions log

Short ADR-style record of choices and why. Append as we evolve.

## D1 — Scope: track external apps, not ourselves (2026-05-31)
Track LLM calls from 5–10 apps using **OpenAI, Gemini, Anthropic**, local or cloud. LightTrack's own
internal calls (its judge, its DB writes) are **not** tracked. *Implication:* multi-provider normalization
+ a price book covering all three; the judge's spend is recorded as a score cost but excluded from traffic
metrics/limits.

## D2 — Dashboards: Looker Studio (2026-05-31)
Use **Looker Studio** (free) over BigQuery rather than building a web UI. *Implication:* keep the cloud
store query-friendly (flat, well-typed `events`); no frontend crate for now.

## D3 — Run local first, e2-micro later (2026-05-31)
Phase A runs on this Windows box; Phase B moves `api`→Cloud Run, `runner`→e2-micro. *Implication:* a `Store`
trait with **SQLite (local)** and **BigQuery+Firestore (cloud)** backends, schemas kept in lockstep.

## D4 — Judge is unbudgeted; limits apply to incoming traffic (2026-05-31)
The scoring/benchmark engine runs without a budget cap. **Limits** (cost/calls/tokens per hour/day/month)
are tripped by **monitored traffic** and produce alerts + an advisory throttle flag. Tier = **Max 20x**.
*Implication:* limit evaluation keyed on `project_id` for ingested events only; judge calls bypass it but
their cost is still recorded for visibility.

## D5 — Keep an MCP server in the product (2026-05-31)
Ship `lighttrack-mcp` so Claude Code/agents can query traces, costs, scores, limit status and trigger
benchmarks. *Implication:* read-mostly tool surface over the `Store`; dogfood from the terminal.

## D6 — API-key security, enforced on e2-micro (2026-05-31)
Per-project API keys (salted-hash at rest, `Bearer lt_<prefix>_<secret>`), admin key for management. Relaxed
`dev` mode locally; enforced once remote. Secrets in Secret Manager (cloud) / git-ignored files (local).

## D7 — Parallel ingest + project management (2026-05-31)
Async axum handles concurrent ingest; writes batched to the Store. First-class **projects** with their own
keys, limits, redaction policy, and scorecards. Target load (≤1k calls/hr) is well within free tiers.

## D8 — Name & shape: "LightTrack", Rust workspace (2026-05-31)
Rust end-to-end to match the user's existing Rust app and keep Cloud Run / e2-micro footprints small.
Cargo workspace: `core` (logic) + `api` / `runner` / `mcp` / `cli` (services). Evolve functionally over days.

## D9 — Scoring engine specifics + cost finding (2026-05-31)
The `engine` crate invokes `claude -p --output-format json --model haiku --json-schema <JudgeVerdict>`
with stdin redirected to null; it reads the verdict from `structured_output` (fallback: extract a JSON
object from the `result` text) and `total_cost_usd` from the envelope.

**FINDING:** plain `claude -p` auto-loads ~40k tokens of context (CLAUDE.md / skills / MCP / system prompt)
and bills the prompt-cache creation, so each judge call costs ~$0.02–0.10 regardless of how small the
judging payload is (measured live: a one-word reply cost $0.051; a real judgement $0.023–0.102).
*Mitigation:* `--bare` skips that auto-loading (cost drops to fractions of a cent), but it bypasses
subscription OAuth and so needs `ANTHROPIC_API_KEY` — exposed as the runner's `--bare` flag for when an
API key is available. This sharpens the [[lighttrack-project]] post-2026-06-15 credit math: budget ≈
included-Agent-SDK-credit ÷ ~$0.05 (non-bare) or much more with `--bare`.

**Windows gotcha:** the npm install provides `claude.cmd`/`claude.ps1` shims (no PATH `claude.exe`); a child
process can't invoke `.cmd` with our quote-heavy `--json-schema` arg (Rust rejects unsafe batch args). The
shim wraps a real `bin/claude.exe`, so the runner auto-resolves that on Windows (override via
`--claude-bin` / `LIGHTTRACK_CLAUDE_BIN`). The judge remains **unbudgeted** (see D4).

## D0 — Billing reality: Claude Code 2026-06-15 (background, drives D4)
From **2026-06-15**, headless `claude -p` / Agent SDK stop drawing on normal subscription limits and meter
against a separate monthly **Agent SDK credit** at API rates (Pro $20 / Max 5x $100 / **Max 20x $200**, no
rollover). LightTrack's judge consumes that credit. Mitigations baked in: pluggable engine, **Haiku-default**
judging, prompt caching, and recording each judge call's `cost_usd` so credit burn is visible.
