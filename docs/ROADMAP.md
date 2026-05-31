# LightTrack — Roadmap

Evolved daily. Checked items are done; the rest is the plan we agreed on.

## Phase 0 — Scaffold ✅ (today)
- [x] Repo, workspace, docs, decisions log
- [x] `core`: normalized `LlmEvent`, `PriceBook` + cost calc, `LimitRule` eval, scoring/benchmark types
- [x] SQLite + BigQuery DDL, `pricing.json`
- [x] `cargo build` green for the whole workspace

## Phase 1 — Ingest → query (local, SQLite) ✅
- [x] `Store` trait + SQLite backend (`rusqlite`, bundled)
- [x] `api`: `POST /v1/events` (normalize + compute cost + write), `GET /v1/events`, `GET /v1/costs`
- [x] Verify: synthetic traffic from 3 fake "apps", cost rollups confirmed against the running server
- [x] Project + API-key model; `dev` (relaxed) vs `enforced` auth  → done in Phase 2
- [ ] Minimal client snippet (Rust + Python) to wrap OpenAI/Anthropic/Gemini calls  → Phase 2.5

## Phase 2 — Projects, keys, limits ✅
- [x] CRUD for projects, API keys (salted-hash), limit rules (via `lt` CLI + API)
- [x] Rolling-window usage + limit evaluation on ingest (cost/calls/tokens × hour/day/month)
- [x] `GET /v1/limits/status` advisory throttle flag; breaches surfaced in the ingest response
- [x] `dev` vs `enforced` auth (admin key + per-project keys); verified 401/403 boundaries
- [x] `lt` CLI (projects/keys/limits/costs/events) — verified against the enforced server
- [~] Inline breach alerts: server-side `[ALERT]` log done; webhook/ntfy/Pub/Sub delivery deferred to Phase 5 (cloud)

## Phase 3 — Scoring engine ✅ (benchmarks pending)
- [x] `engine` crate: `claude -p --output-format json --model <m> --json-schema <JudgeVerdict>`
      (stdin=null; structured_output with result-text JSON fallback); parses verdict + `total_cost_usd`
- [x] `runner` (`lt-runner`): `score` (judge recent events) + `score-text` (ad-hoc); posts to `/v1/scores`;
      Windows `claude.exe` auto-resolution; `--bare` option for cheap judging
- [x] `api`: `POST/GET /v1/scores`, `GET /v1/events/:id`
- [x] Verified live: Haiku judge scored a correct answer 1.0/pass and a wrong answer 0.0/fail,
      scores persisted with judge cost
- [x] `BenchmarkDefinition` + run + scorecard + regression baseline  → see Phase 3.5
- [ ] Scheduled online sampling of live events (cron)  → Phase 5 / cron

## Phase 3.5 — Benchmarks ✅
- [x] `core`: `BenchmarkCase` + inline `dataset` on `Benchmark`; serde defaults on `Benchmark`/`BenchmarkRun`
- [x] `store`: create/get/list benchmarks + create/list runs (dataset stored inline as JSON)
- [x] `api`: `POST/GET /v1/projects/:id/benchmarks`, `GET /v1/benchmarks/:id`, `GET /v1/benchmarks/:id/runs`, `POST /v1/benchmark-runs`
- [x] `engine`: `build_eval_prompt` (rubric + optional reference answer)
- [x] `runner`: `lt-runner bench --benchmark <id>` — judge each case, aggregate mean/pass-rate/cost, compare to baseline, record a run + per-case scores
- [x] `mcp`: `list_benchmarks`, `get_benchmark_runs` tools
- [x] Verified live: 3-case `capitals-qa` (2 correct, 1 wrong) → mean 0.667, `regressed` vs 0.9 baseline; run + 3 scores stored; surfaced via MCP

## Phase 3.6 — Benchmark framework hardening (design: docs/BENCHMARK_FRAMEWORK.md)
- [x] 3.6a Cost foundation: DB-backed `model_prices` (seeded with official 2026-05-31 rates) +
      `GET /v1/prices` + `PUT /v1/prices/:provider/:model` (live hot-swap, no restart); judge latency +
      tokens captured in the engine; runs record p50/p95 latency, total tokens, cost.
      *Verified:* live price update $1→$2 mid-run; run stored p50=8511ms, tokens=123742.
- [x] 3.6b Datasets from real events + hybrid anonymization: `anon` crate (regex PII scrubber →
      typed placeholders); `datasets`/`dataset_items` tables + API (`POST/GET /v1/projects/:id/datasets`,
      `GET /v1/datasets/:id`, `POST/GET /v1/datasets/:id/items`, `POST /v1/datasets/:id/freeze`);
      `lt-runner dataset build --from events --project --n [--llm-scrub]` (regex always, optional
      `claude -p` pass for names/free-text); benchmarks can run a stored dataset via `dataset_ref`.
      *Verified:* 2 PII events → frozen dataset, 9 redactions (EMAIL/PHONE/CC/IP/IBAN/SSN/SECRET), no
      raw PII; frozen→409; benchmark resolved cases from the dataset and scored them.
- [ ] 3.6c Golden-standard rubric methodology (weighted anchored dimensions) + report & healing
- [ ] 3.6d Async benchmark job queue (jobs table + `lt-runner serve`)
- [ ] 3.6e Multi-provider generation (Claude via `claude -p` now; OpenAI/Gemini when keyed)

## Phase 4 — MCP ✅
- [x] `mcp` (`lt-mcp`): hand-rolled JSON-RPC 2.0 stdio server (no SDK); thin HTTP client of the API
- [x] Tools: `list_projects`, `get_cost_summary`, `query_events`, `get_limit_status`, `list_scores`
- [x] Verified via a real JSON-RPC session (initialize → tools/list → tools/call returning live data)
- [x] `.mcp.json` committed for project-scoped registration in Claude Code
- [x] Benchmark read tools (`list_benchmarks`, `get_benchmark_runs`) added in Phase 3.5
      (triggering a run stays in `lt-runner`, which has the `claude -p` engine; MCP is read-only)

## Phase 5 — Cloud move
- [ ] BigQuery `Store` backend + Firestore config backend
- [ ] Containerize `api` → Cloud Run; `runner` → e2-micro; Secret Manager for keys
- [ ] Pub/Sub job dispatch; Cloud Scheduler periodic checks → Cloud Function alerts
- [ ] Looker Studio dashboard on BigQuery
- [ ] Enforce API-key auth + TLS

## Parallelism & scale targets
- Expected: 5–10 apps × 10–100 calls/hour ≈ ≤1k calls/hr. `api` handles ingest concurrently (async axum);
  batched writes to the Store. Comfortably inside every GCP free-tier ceiling.
