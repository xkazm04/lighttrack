# LightTrack — Universal Benchmark & Evaluation Framework

Design for hardening the benchmark layer into a reusable, opinionated evaluation framework. Goal: give
teams a *solid default methodology* for LLM evaluation (datasets from real traffic, multi-provider
comparison, a rigorous LLM-as-judge with reports + remediation), since most teams have none.

Status: **design** (extends the shipped Phase 3.5 benchmarks). Drives sub-phases 3.6a–3.6e below.

## 0. Concepts (vocabulary)
- **Dataset** — a versioned set of **DatasetItems** `{input, expected?, context?, tags, source_event_id?}`.
  Built by hand, imported, or **sampled from real events** (with anonymization).
- **PromptVariant** — a named system/instruction prompt under test (`v1`, `v2`, …).
- **Target** — a thing that produces an output: `{provider, model, prompt_variant}`. A benchmark compares
  many targets.
- **Run** — one execution of a benchmark over a dataset for one or more targets, producing **CaseResults**.
- **Rubric** — weighted **dimensions**, each with anchored 0–1 levels; the judge's contract.
- **Report** — aggregated scorecard per target/dimension + **recommendations & "healing"** (remediation).

## 1. Datasets from real data, with anonymization  (#1)
Pipeline: `sample → anonymize → review → freeze`.
- **Sampling** from `events`: by project, time window, model, status, tag; strategies = `recent`, `random`,
  `stratified` (balance by model/outcome), `errors-only`. Target N items; dedupe near-identical inputs.
- **Anonymization** (PII scrub) runs on `input`/`output`/`context` before an item is stored in a dataset:
  - **Heuristic pass (always):** regex for emails, phone numbers, credit cards (Luhn), IBANs, IPs, URLs,
    API keys/secrets, national IDs; replace with typed placeholders (`<EMAIL>`, `<PHONE>`…).
  - **LLM pass (optional):** a `claude -p` call that catches names/orgs/locations/free-text PII the regex
    misses, preserving meaning. Costs per item (judge-engine economics, see DECISIONS D9).
  - Each item records `anonymization: {method, placeholders_count}` for auditability. Original text is
    never copied into the dataset; only the scrubbed version.
- **Output:** a frozen, versioned dataset (immutable once `frozen=true`) so runs are comparable over time.

## 2. Multi-provider / multi-prompt comparison  (#2)
A benchmark defines a **matrix** of targets = `{providers × models} × {prompt variants}`. For each
DatasetItem × target, the framework **generates** an output, then **judges** it.

- **Provider abstraction** (`Generator` trait): `anthropic` (via `claude -p` or API), `openai`, `google`.
  Each needs credentials; see *Open decisions*.
- **Generation vs judging are separated.** The judge model should differ in family from the generator to
  avoid **self-preference bias** (§3). Default judge = Claude Haiku via `claude -p`; when judging Claude
  outputs, prefer pairwise + randomized order, or a neutral judge.
- **Output:** a comparison table — for each dimension and overall: score, pass-rate, **p50/p95 latency**,
  **tokens**, **$ cost** — so "best" is a quality/latency/cost trade-off, not just quality.

## 3. Golden-standard LLM-as-judge methodology  (#3)  ← the core
A clear, defensible scoring system. Defaults encode current best practice (see Sources).

**Rubric.** A rubric is weighted **dimensions** (e.g. *correctness 0.5, completeness 0.2, faithfulness 0.2,
concision 0.1*). Each dimension scores on a **narrow anchored scale** normalized to 0–1, with explicit
level descriptions ("1.0 = fully correct & verifiable; 0.5 = minor error; 0 = wrong/unsupported"). Overall
= weighted sum. Pass = overall ≥ threshold AND no gating dimension below its floor.

**Modes** (pick per goal):
- **Pointwise** (analytic, per-item) — monitoring, dashboards, regression tracking. *(shipped)*
- **Reference-guided** — when `expected` exists, anchor the score to the golden answer. *(shipped: eval prompt)*
- **Pairwise** — A/B between two targets, "which better satisfies the rubric"; best for model/prompt
  *selection* and release gates. Aggregated over items with randomized slot order.

**Judge prompt = RCAF:** Role (impartial judge) · Context (rubric + reference) · Action (score each
dimension with reasoning, then overall) · Format (strict JSON schema). We already use `--json-schema`.

**Calibration & reliability:**
- Narrow anchored scales; few-shot anchor examples (low/med/high) per rubric when available.
- **Self-consistency:** sample the judge k times (or k judges), report mean + agreement; low agreement →
  flag the item as ambiguous rather than trusting one score.
- A **golden/calibration set** of human-labeled items measures judge↔human agreement (Cohen's κ /
  correlation); a rubric isn't "trusted" until agreement clears a bar. ✅ shipped: `lt-runner
  calibrate --file <jsonl> --rubric "<criteria>" | --rubric-id <id>` re-judges each labeled
  `{input, output, human_score}`, then reports Cohen's κ + Pearson + MAE/RMSE + judge-vs-human bias
  and a TRUSTED/NOT-TRUSTED verdict against `--kappa-bar` (default 0.6). Judge-only (no generation),
  self-contained (no Store/schema changes). Agreement math lives in `core::calibration` (unit-tested).

**Bias controls (the four):** position → randomize/shuffle A/B and aggregate; verbosity → rubric explicitly
penalizes unnecessary length; self-preference → judge family ≠ generator family (or pairwise+neutral);
authority → strip provider/model identity from what the judge sees.

**Report & "healing":** per-target × per-dimension scorecard; **failure clustering** (group low-scoring
cases by dimension/pattern); **recommendations** — concrete, actionable: e.g. "completeness lowest on
multi-part questions → add a checklist step to prompt v2", "switch judge off same-family to cut
self-preference", "Haiku within 3% of Sonnet at 1/5 cost → prefer Haiku". Regression vs baseline +
quality/cost/latency trade-off called out explicitly.

## 4. Async benchmark queue (non-blocking)  (#4)
Benchmark runs must never block ingestion. A **jobs** table + a worker loop in `lt-runner`:
- `POST /v1/benchmark-runs:enqueue` inserts a `job {type: bench_run, payload, status: queued}` and returns
  immediately. Ingestion (`POST /v1/events`) is unaffected.
- `lt-runner serve` polls `GET /v1/jobs?status=queued&claim=1` (atomic claim → `running`), executes
  (generate?/judge/aggregate), posts results, marks `done`/`failed` with progress + error.
- States: `queued → running → done|failed`; heartbeat + `attempts` for retry; concurrency cap so judge
  calls don't stampede. **Cloud:** swap the jobs table for Pub/Sub; same worker.

### 4a. Self-running benchmarks (opt-in recurrence)
By default a benchmark runs **only** on a manual enqueue (`POST /v1/benchmarks/:id/enqueue`) or a
prompt-version cut (the registry auto-enqueues) — there is no cron in the benchmark path. Turn a
benchmark into **continuous quality monitoring** by giving it a recurrence interval:

```bash
# create a recurring benchmark: re-run itself ~every hour
curl -sX POST "$LIGHTTRACK_URL/v1/projects/$PID/benchmarks" -H "authorization: Bearer $KEY" \
  -H 'content-type: application/json' \
  -d '{ "name": "support-quality", "rubric": "…", "dataset_ref": "online-latest",
        "baseline_score": 0.8, "schedule_interval_secs": 3600 }'
```

- **Storage:** `schedule_interval_secs` rides inside the benchmark's free-form `target` JSON — **no
  schema/column change** (benchmarks are fixed-column rows; `target` is the only free-form field, and
  it round-trips unchanged through SQLite *and* Postgres). It is therefore **not supported alongside a
  comparison-matrix** benchmark (an array `target`/`targets` has no room for it) — that combination is
  rejected with a `400`. Use a single-target, rubric, or simple benchmark for recurrence.
- **Enable / disable:** set the interval to enable, `0`/unset to disable. There is no `PATCH` surface
  today, so "changing" recurrence on an existing benchmark means recreating it (v1 story).
- **Who runs it:** `lt-runner serve` performs a **recurrence sweep** on a subsampled cadence
  (`--recur-interval`, default 60s; `0` disables). A benchmark is **due** when (a) it has an interval,
  (b) it has **no** queued/running `bench_run` job, and (c) its most recent run's `finished_at` (or
  `started_at` fallback) is older than the interval. The sweep enqueues a normal `bench_run` (reusing
  the existing job path) — the same worker then claims and runs it. This is **idempotent**: an
  in-flight job or a recent run means "not due", so repeated sweeps never pile up jobs.
- **OS cron instead of a daemon:** `lt-runner serve --once` runs exactly one sweep + claims one job,
  so an external scheduler can drive recurrence:

  ```cron
  */15 * * * *  cd /srv/lighttrack && lt-runner serve --once >> /var/log/lt-serve.log 2>&1
  ```

  Running "too often" is harmless — the due-check keeps it idempotent. Discovery uses existing read
  endpoints (list projects → list benchmarks → list runs/jobs) with the runner's admin key.

## 5. Latency + token cost, DB-backed price table  (#5)
- **Per-call metrics** captured on generation/judge: `latency_ms`, `input/output/cached tokens`, `cost_usd`.
  Aggregated into runs as p50/p95 latency, total tokens, total $.
- **`model_prices` table** (replaces `pricing.json` as source of truth; JSON becomes the seed/bootstrap):
  `provider, model, input_per_mtok, output_per_mtok, cached_input_per_mtok, effective_date, source_url`.
  Seeded from official pages (researched 2026-05-31 — see `config/pricing.json` sources). Cost is computed
  from the row whose `effective_date` ≤ the event time (price history preserved). **Prompt-length
  tiers** (e.g. Gemini Pro >200k) and **batch/flex** rates are supported via variant rows
  (`<model>@in>N`, `<model>@batch`, `<model>@flex`) — no schema change; see `docs/PRICING.md`.
- API: `GET /v1/prices`, `PUT /v1/prices/:provider/:model` (admin) so prices update without redeploys.

## 6. Collective Model Intelligence Network (opt-in network effect)  (#6)
Every instance benchmarks models on *its own real tasks*. The network turns those private scorecards into
a **shared, real-world model leaderboard** — quality × cost × latency per `(provider, model, task_type)` —
so model selection rests on collective field data, not vendor marketing benchmarks. The more teams run
LightTrack, the better the data for everyone (the moat).

- **Privacy-safe by construction** (`core::collective`, pure + unit-tested):
  - *Aggregate-only inputs.* A digest is built from benchmark **run scorecards**, which already carry no
    prompt/response text — the builder never touches `events`. No project ids, customer ids, or free text
    ever enter a digest.
  - *k-anonymity.* A `(provider, model, task_type)` bucket is published only when it aggregates ≥
    `min_cases` cases (default 5), so a rare/unique task can't be fingerprinted to one operator.
  - *Coarse task types.* A benchmark name is classified into a **fixed vocabulary** (`qa`,
    `summarization`, `coding`, `rag`, …) — the raw name is never published.
  - *Opaque contributor id.* `LIGHTTRACK_COLLECTIVE_ID` is hashed (SHA-256, truncated) before it goes on
    the wire, so a hub can update a source idempotently without learning who it is; unset ⇒ `anonymous`.
- **Topology.** Any LightTrack can be a **hub** (`LIGHTTRACK_COLLECTIVE_ACCEPT=1`, off by default) that
  receives digests and merges them; others contribute. Same binary, no central service required.
- **API.** `GET /v1/collective/digest?min_cases=` (admin — preview what we'd publish) ·
  `POST /v1/collective/ingest` (hub-only; replaces a contributor's set, validates + clamps each entry) ·
  `GET /v1/collective/leaderboard?task_type=&provider=` (open read — the merged leaderboard).
- **Surfaces.** `lt collective leaderboard|digest|contribute --hub <url>` (the CLI does the two-hop push:
  GET own digest → POST to the hub); the `get_collective_leaderboard` MCP read tool; a rendered
  leaderboard table shared by CLI + MCP.

## Data model additions (SQLite ↔ BigQuery, behind the Store trait)
```
datasets(id, project_id, name, version, frozen, source, created_at)
dataset_items(id, dataset_id, input, expected?, context?, tags, source_event_id?, anonymization)
prompt_variants(id, project_id, name, label, system_prompt)
targets(id, benchmark_id, provider, model, prompt_variant_id?)   -- the matrix
benchmark_runs(... + target_id?, p50_latency_ms, p95_latency_ms, total_tokens, total_cost_usd, report)
case_results(id, run_id, dataset_item_id, target_id, output, latency_ms, tokens, cost_usd, scores_json)
rubrics(id, project_id, name, dimensions_json, threshold)        -- weighted anchored dimensions
model_prices(provider, model, input_per_mtok, output_per_mtok, cached_input_per_mtok, effective_date, source_url)
jobs(id, type, payload_json, status, attempts, progress, error, claimed_at, created_at)
collective_entries(contributor_id, provider, model, task_type, quality, pass_rate, avg_cost_usd,
                   p50_latency_ms?, p95_latency_ms?, n_runs, n_cases, received_at)  -- hub side; PK=(contributor_id,provider,model,task_type)
```

## Phased plan
- **3.6a — Cost foundation:** `model_prices` table (seed from researched prices) + `GET/PUT /v1/prices`;
  capture latency + tokens + cost on judge calls; add p50/p95 latency + tokens + $ to runs. *(no new deps)*
- **3.6b — Datasets + anonymization:** datasets/dataset_items; `lt dataset build --from-events` (regex
  scrub + optional `--llm-scrub`); freeze/version.
- **3.6c — Rubric methodology + report:** `rubrics` (weighted anchored dimensions); per-dimension judging;
  self-consistency (k-sample); golden/calibration agreement; report with recommendations & healing.
- **3.6d — Async queue:** `jobs` table + `lt-runner serve` worker; enqueue endpoint; non-blocking.
- **3.6e — Multi-provider generation:** `Generator` trait + OpenAI/Gemini/Anthropic clients; target matrix;
  comparison report (quality × latency × cost). *(needs provider API keys)*

## Decisions (resolved 2026-05-31) & status
1. **Generation mode** = Claude-now via `claude -p`; OpenAI/Gemini behind `engine::generate` and activate
   when keyed (return a clear error until then). ✅ shipped in 3.6e.
2. **Anonymization** = hybrid: regex always + optional `--llm-scrub` pass. ✅ shipped in 3.6b.

**All sub-phases 3.6a–3.6e are implemented, tested, and verified live** (see ROADMAP). The **Gemini and
OpenAI generation adapters are now live too** (reqwest/native-tls, keys from `.env`, gen cost priced from
the DB book) — verified in a 3-way Claude/Gemini/OpenAI comparison. The **judge↔human calibration set is
now shipped too** (`lt-runner calibrate`; Cohen's κ + correlation + trust verdict — see §3).
**Prompt-length-tiered & batch/flex pricing** is shipped via price-row variants (`docs/PRICING.md`).
Remaining future work: BigQuery analytical sink + Pub/Sub queue (Phase 5/packaging).

## Sources (researched 2026-05-31)
- Anthropic API pricing — https://platform.claude.com/docs/en/about-claude/pricing
- OpenAI API pricing — https://developers.openai.com/api/docs/pricing
- Google Gemini API pricing — https://ai.google.dev/gemini-api/docs/pricing
- LLM-as-judge best practices — https://futureagi.com/blog/llm-as-judge-best-practices-2026 ·
  https://www.comet.com/site/blog/llm-as-a-judge/ ·
  Rubric-based evals & position bias — https://arxiv.org/pdf/2602.02219
