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
- [x] Client ingestion SDKs (`clients/`): **Python**, **TypeScript**, **Rust** — thin fire-and-forget
      libraries that wrap OpenAI/Anthropic/Gemini results and POST to `/v1/events` (non-blocking,
      best-effort, env-configured). Python = stdlib-only background thread; TS = global `fetch`, zero
      deps; Rust = standalone crate reusing `lighttrack-core::LlmEvent` (no payload drift). *Verified
      live:* all three ingested into a running API (13 events; provider/model/cost correct).

## Phase 2 — Projects, keys, limits ✅
- [x] CRUD for projects, API keys (salted-hash), limit rules (via `lt` CLI + API)
- [x] Rolling-window usage + limit evaluation on ingest (cost/calls/tokens × hour/day/month)
- [x] `GET /v1/limits/status` throttle flag; breaches surfaced in the ingest response
- [x] Ingest admission control: `throttle`/`block` breaches reject the event with 429 atomically
      (check-then-insert in one store step — concurrent bursts can't race past the cap)
- [x] `dev` vs `enforced` auth (admin key + per-project keys); verified 401/403 boundaries
- [x] `lt` CLI (projects/keys/limits/costs/events) — verified against the enforced server
- [x] Inline breach alerts: server-side `[ALERT]` log + **delivery to webhook / ntfy** on breach
      (`LIGHTTRACK_ALERT_WEBHOOK` / `_NTFY` / `_COOLDOWN_SECS`). Best-effort, off the request path
      (spawned task), deduped per (project, metric, window) by a cooldown. Webhook payload carries
      `text` (Slack) + `content` (Discord) + structured `breach`. See `docs/ALERTS.md`. *Verified live:*
      3 breaching ingests → exactly 1 webhook + 1 ntfy delivered (dedup). Pub/Sub fan-out is Phase 5.

## Phase 3 — Scoring engine ✅ (benchmarks pending)
- [x] `engine` crate: `claude -p --output-format json --model <m> --json-schema <JudgeVerdict>`
      (stdin=null; structured_output with result-text JSON fallback); parses verdict + `total_cost_usd`
- [x] `runner` (`lt-runner`): `score` (judge recent events) + `score-text` (ad-hoc); posts to `/v1/scores`;
      Windows `claude.exe` auto-resolution; `--bare` option for cheap judging
- [x] `api`: `POST/GET /v1/scores`, `GET /v1/events/:id`
- [x] Verified live: Haiku judge scored a correct answer 1.0/pass and a wrong answer 0.0/fail,
      scores persisted with judge cost
- [x] `BenchmarkDefinition` + run + scorecard + regression baseline  → see Phase 3.5
- [x] Scheduled online sampling of live events: `lt-runner schedule --project <id> [--interval s |
      --once] [--n N --name-prefix p --llm-scrub]` periodically samples recent events → scrubs PII →
      freezes a dataset. Idempotent (dataset named after the newest sampled event; idle cycles skip,
      even across `--once` runs), so it works as a daemon or under OS cron / systemd / Task Scheduler /
      Cloud Scheduler (see `docs/SCHEDULING.md`). *Verified live:* build → skip-when-idle → rebuild on
      new traffic, 2 distinct datasets, PII redacted.

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
- [x] **Prompt-length tiers + batch/flex pricing**: variant price rows (`<model>@in>N`,
      `<model>@batch`, `<model>@flex`) resolved in `core::PriceBook` by input size / call mode
      (`metadata.pricing_mode` or a `batch`/`flex` tag). No schema/Store change — managed via the same
      `PUT /v1/prices`. See `docs/PRICING.md`. *Verified live:* tier below/above 200k (0.125 vs 0.75),
      batch via tag and via metadata (6.25 vs standard 12.5).
- [x] 3.6b Datasets from real events + hybrid anonymization: `anon` crate (regex PII scrubber →
      typed placeholders); `datasets`/`dataset_items` tables + API (`POST/GET /v1/projects/:id/datasets`,
      `GET /v1/datasets/:id`, `POST/GET /v1/datasets/:id/items`, `POST /v1/datasets/:id/freeze`);
      `lt-runner dataset build --from events --project --n [--llm-scrub]` (regex always, optional
      `claude -p` pass for names/free-text); benchmarks can run a stored dataset via `dataset_ref`.
      *Verified:* 2 PII events → frozen dataset, 9 redactions (EMAIL/PHONE/CC/IP/IBAN/SSN/SECRET), no
      raw PII; frozen→409; benchmark resolved cases from the dataset and scored them.
- [x] 3.6c Golden-standard rubric methodology: `rubrics` (weighted anchored dimensions + gating floors,
      pass threshold) + API CRUD; engine builds a per-dimension JSON schema (RCAF prompt, verbosity/
      self-preference guards) and computes weighted overall + pass itself; self-consistency (`--samples`
      k-vote agreement); `lt-runner bench` rubric mode → per-dimension scorecard + run `report`
      (dimension means, weakest dimension, failing-case clustering, recommendations, optional `--heal`
      LLM paragraph). *Verified:* good answer 1.0/pass, fragmented answer 0.56/fail (completeness 0.35),
      weakest=completeness, report + recommendations stored.
- [x] 3.6d Async benchmark job queue: `jobs` table (queued→running→done/failed, attempts/retry,
      stale-claim recovery, progress/result) + `core::Job`; API `POST /v1/benchmarks/:id/enqueue`
      (returns immediately), `GET /v1/jobs[/:id]`, worker endpoints `POST /v1/jobs/claim|:id/progress|:id/finish`;
      `lt-runner serve [--once --interval --stale-secs]` claims→runs→finishes with retry up to max_attempts.
      *Verified:* enqueue returned instantly while ingest kept serving; worker ran the bench and marked the
      job `done` with a result; run persisted.
- [x] 3.6e Multi-provider/multi-prompt generation: `core::BenchTarget` (provider+model+system-prompt
      variant, stored inline in the benchmark `target` field); `engine::generate` (Claude via `claude -p`
      with target model + `--append-system-prompt`; OpenAI/Gemini return a clear "needs HTTPS adapter +
      key" error until enabled); `lt-runner bench` compare mode generates per target, judges with a fixed
      judge (rubric or freeform), records a run per target, prints a quality × cost × latency table + best.
      *Verified:* concise vs verbose Claude prompt variants (1.0 vs 0.5 on a concision rubric), OpenAI
      target skipped gracefully without a key. **Phase 3.6 benchmark framework complete.**

## Provider generation adapters (post-3.6) ✅
- [x] **Gemini** (`generativelanguage … :generateContent`) + **OpenAI** (`/v1/chat/completions`) generation
      live in `engine::generate` via reqwest (native-tls / SChannel on Windows). Keys loaded from `.env`
      (dotenvy): `GEMINI_API_KEY` / `OPENAI_API_KEY`; generation cost priced from the DB price book by tokens.
      *Verified live:* 3-way compare (claude-haiku / gemini-2.5-flash / gpt-4o-mini) — all correct on a
      capital-of-France case; gemini fastest (744ms), gpt-4o-mini cheapest gen ($0.00001).
- [x] **Provider-configurable judge**: `judge_model` accepts `[provider/]model` (e.g. `google/gemini-2.5-flash`,
      `openai/gpt-4o-mini`; bare name ⇒ anthropic/`claude -p`). The judge is now just a structured
      generation parsed via `generate()`, so any provider can judge (mitigating self-preference vs the
      generator family). Judge cost priced from the DB book when the provider returns no $.
      *Verified live:* same answer judged by Gemini and by OpenAI (both 1.0/pass, judge cost priced).

## Judge calibration (post-3.6) ✅
- [x] `core::calibration` — pure agreement math (Cohen's κ on pass/fail, Pearson, MAE/RMSE, judge-vs-human
      bias, trust verdict vs a κ bar); unit-tested (perfect/total-disagreement/bias/empty).
- [x] `lt-runner calibrate --file <jsonl|json> --rubric "<criteria>" | --rubric-id <id>
      [--threshold 0.7 --kappa-bar 0.6 --samples k --report out.json]` — re-judges each human-labeled
      `{input, output, human_score}` and reports per-item agreement + aggregate κ/correlation. Judge-only,
      self-contained (no Store/schema/API changes — works against the existing judge engine).
      `config/calibration.example.jsonl` ships as a starter set.
- [x] *Verified live (Haiku judge, 8 items):* κ=0.750 (TRUSTED), pearson=0.985, MAE=0.037, bias=+0.037;
      the lone miss correctly flagged the judge under-penalizing a verbose-but-correct answer vs the human.

## Phase 4 — MCP ✅
- [x] `mcp` (`lt-mcp`): hand-rolled JSON-RPC 2.0 stdio server (no SDK); thin HTTP client of the API
- [x] Tools: `list_projects`, `get_cost_summary`, `query_events`, `get_limit_status`, `list_scores`
- [x] Verified via a real JSON-RPC session (initialize → tools/list → tools/call returning live data)
- [x] `.mcp.json` committed for project-scoped registration in Claude Code
- [x] Benchmark read tools (`list_benchmarks`, `get_benchmark_runs`) added in Phase 3.5
- [x] **Expanded** (commit 088b77c): modularized (`client`/`rpc`/`read`/`write`/`tools` + wiring `main`);
      grown to 18 read tools (events/costs/scores/prices/limits/projects/benchmarks+runs/datasets+items/
      rubrics/jobs, all `readOnlyHint`) + 9 write tools (`enqueue_benchmark` + create project/dataset/item/
      freeze/rubric/benchmark/limit + `put_price`). Writes are OFF by default, gated behind
      `LIGHTTRACK_MCP_ALLOW_WRITES` atop the API's admin checks; key-minting deliberately not exposed.
      Fulfils D5 ("trigger benchmarks"). Verified: read-only hides/blocks writes; write mode runs the full
      create→freeze→benchmark→enqueue→poll loop; frozen-dataset writes still return 409 via the API.

## Phase 5 — Packaging & multicloud (design: docs/PACKAGING.md)
Reframed from "GCP cloud move" to portable, multi-DB/multicloud: one image + one
`LIGHTTRACK_DATABASE_URL` DSN; `Store`-trait adapters; tiered deploy (compose → Terraform → Helm).
- [x] 5a **Postgres backend** (`lighttrack-store-pg`, sqlx): full `Store` parity; `claim_job` via
      `FOR UPDATE SKIP LOCKED`. Selected by `LIGHTTRACK_DATABASE_URL=postgres://…`. Verified vs PG 16.
- [x] **Firestore backend** (`lighttrack-store-firestore`, REST-over-reqwest): full parity, modular
      (rest/codec + per-domain). `LIGHTTRACK_DATABASE_URL=firestore://<project>`. See `docs/FIRESTORE.md`.
- [x] 5b **Containerize → GHCR**: multi-stage `Dockerfile` (all 4 bins), compose, buildx CI; image
      `ghcr.io/xkazm04/tracklight:v0.0.1` published (amd64).
- [x] 5f **Binary installers + dashboards**: cargo-dist config + `deploy/install.{sh,ps1}`; Grafana
      dashboard over Postgres (`dashboards/grafana`, `docker-compose.postgres.yml`).
- [~] 5d/5e **Helm chart + GCP/Azure Terraform modules** (`deploy/helm`, `deploy/terraform/modules`):
      scaffolded; **unverified** (no helm/terraform locally, no real cloud creds applied yet).
- [ ] Remaining: **Store-conformance tests** across backends (gated on a PG/emulator env var — pg &
      firestore currently have 0 automated tests); validate against real cloud Postgres (Neon/Supabase)
      + apply Helm/TF with GCP+Azure creds; rebuild/publish a **Postgres-capable image** (v0.0.2) and make
      the GHCR package public; AWS App Runner TF module; neutral Queue/Blob/Secrets/Notifier adapters
      (5c); Pub/Sub queue + Cloud Scheduler; optional BigQuery analytical sink (Looker Studio); enforce
      auth + TLS termination in the cloud deploy.

## Parallelism & scale targets
- Expected: 5–10 apps × 10–100 calls/hour ≈ ≤1k calls/hr. `api` handles ingest concurrently (async axum);
  batched writes to the Store. Comfortably inside every GCP free-tier ceiling.
