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
  avoid **self-preference bias** (§3) — now detected and recorded per run, not just advised. Default
  judge = Claude Haiku, via the bare Anthropic Messages API when `ANTHROPIC_API_KEY` is set and via
  `claude -p` otherwise (D12); when judging Claude outputs, prefer pairwise + randomized order, or a
  neutral judge.
- **Output:** a comparison table — for each dimension and overall: score, pass-rate, **p50/p95 latency**,
  **tokens**, **$ cost** — so "best" is a quality/latency/cost trade-off, not just quality.

## 3. Golden-standard LLM-as-judge methodology  (#3)  ← the core
A clear, defensible scoring system. Defaults encode current best practice (see Sources).

**Rubric.** A rubric is weighted **dimensions** (e.g. *correctness 0.5, completeness 0.2, faithfulness 0.2,
concision 0.1*). Each dimension scores on a **narrow anchored scale** normalized to 0–1, with explicit
level descriptions ("1.0 = fully correct & verifiable; 0.5 = minor error; 0 = wrong/unsupported"). Overall
= weighted sum. Pass = overall ≥ threshold AND no gating dimension below its floor.

**Dimension kinds — deterministic scorers in the same pipeline.** Not every dimension needs an
opinion. A dimension carries a `kind`, defaulting to `llm`; every other kind is a **mechanical check
the engine runs locally, at zero tokens and zero cost**, whose 1.0/0.0 verdict then flows through the
*same* weighting, floors, threshold and aggregation as an LLM dimension. The field is additive and
defaulted, so a rubric written before kinds existed deserializes — and re-serializes — unchanged as
all-`llm`.

| `kind` | passes (1.0) when | `check` fields |
|---|---|---|
| `llm` *(default)* | the judge model says so | — |
| `exact` | the output equals the target | `expect` (defaults to the case's `expected`), `case_sensitive`, `trim`, `path` |
| `contains` | the target occurs in the output | same as `exact` |
| `regex` | `pattern` matches anywhere in the output | `pattern` *(required)*, `case_sensitive`, `trim`, `path` |
| `numeric` | the output's number is within `tolerance` of the target | `expect`, `tolerance` (absolute, default 0), `path` |
| `json_valid` | the output parses as JSON (and, with `expect`, carries that value at `path`) | `expect`, `path`, `case_sensitive` |

`check.path` is a JSON Pointer (e.g. `/data/city`) narrowing a JSON output before the check; `numeric`
falls back to the first numeric token in the text, so *"The total is 41.95 dollars."* is comparable.

```json
{ "name": "extraction", "threshold": 0.8, "dimensions": [
  { "key": "city",     "description": "the extracted city", "weight": 3.0, "floor": 1.0,
    "kind": "exact",      "check": { "path": "/city" } },
  { "key": "wellformed","description": "valid JSON envelope", "weight": 1.0,
    "kind": "json_valid" },
  { "key": "total",    "description": "the summed amount", "weight": 2.0,
    "kind": "numeric",    "check": { "expect": "42", "tolerance": 0.1, "path": "/total" } },
  { "key": "tone",     "description": "professional, no filler", "weight": 1.0 }
] }
```

Rules that make a mixed rubric honest:
- **Not narrated, not double-counted.** The judge prompt and its JSON schema list *only* the `llm`
  dimensions, so the model never re-scores something already decided mechanically.
- **Agreement is an LLM-only statement.** A deterministic dimension is evaluated once and is exactly
  reproducible; folding it into cross-sample `agreement` would drag every rubric toward 1.0 and hide
  the judge's real instability. So `agreement` (and `samples_parsed` / `parse_failures`) covers the
  sampled LLM dimensions alone — while the `overall` covers every dimension.
- **Auditable.** Each mechanical dimension records *why* in its `detail.reasoning`, e.g.
  ``numeric: expected `42`, got `41.6`, tolerance 0.1 → fail``.
- **Operator errors are loud.** A `regex` with no pattern, or an `exact` with neither `check.expect`
  nor a case `expected`, is a hard error naming the dimension — never a candidate silently scored 0.
- **An all-deterministic rubric makes no provider call at all**: `samples = 0`, `cost_usd = null`,
  `scored_by = "deterministic"`, determinism `exact`.

**Modes** (pick per goal):
- **Pointwise** (analytic, per-item) — monitoring, dashboards, regression tracking. *(shipped)*
- **Reference-guided** — when `expected` exists, anchor the score to the golden answer. *(shipped: eval prompt)*
- **Pairwise** — A/B between two targets, "which better satisfies the rubric"; best for model/prompt
  *selection* and release gates. Aggregated over items with randomized slot order.

**Judge prompt = RCAF:** Role (impartial judge) · Context (rubric + reference) · Action (score each
dimension with reasoning, then overall) · Format (strict JSON schema). We already use `--json-schema`.

**Prompt-injection defense (D10).** The candidate output is untrusted by construction, so every judge
prompt fences input/reference/output behind **per-call nonce delimiters** and states that only those
boundaries are authoritative. Content imitating a marker is neutralized (`[lt-escaped]`) rather than
passed through, and raises `injection_suspected` on the outcome — so "this case tried to dictate its own
score" is a reportable fact, not an invisible one.

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

**Verdict provenance (D11).** Every posted score carries a nullable `detail`: per-dimension
`{value, weight, floor, floor_hit, reasoning[]}` with **one reasoning per sample that parsed** (all k
retained — they were billed), plus agreement, `samples_requested`/`samples_parsed`/`parse_failures`,
`position_bias` and `injection_suspected`. `GET /v1/scores` returns it. Stored provenance is bounded by
`ScoreDetail::capped()` (≤600 chars/reasoning, ≤8 reasonings/dimension, ≤32 dimensions); SQLite persists
it, other backends read it back as `None`.

**Bias controls (the four):** position → randomize/shuffle A/B and aggregate; verbosity → rubric explicitly
penalizes unnecessary length; self-preference → judge family ≠ generator family (or pairwise+neutral);
authority → strip provider/model identity from what the judge sees.

*Self-preference is enforced (D12), not just recommended.* `engine::same_family` compares the coarse lab
family of judge and target (the model name outranks the provider, so a gateway serving Claude is still
the Anthropic family). Compare and pairwise modes warn on a same-family pairing and record it on the run
(`self_preference` / `self_preference_targets`). It is a warning, never a failure — a same-family run is
sometimes exactly what you mean to measure.

**Determinism stamp (D12).** A verdict should be a measurement, so every outcome records how reproducible
it actually was: `exact` when every sampling control the provider exposes was pinned including a seed
(OpenAI, Gemini), `best-effort` otherwise — the Anthropic Messages API has no `seed`, and `claude -p` has
no sampling knobs at all. The stamp appears in run reports and in each score's `detail`; a run takes its
weakest call's stamp. **On a `best-effort` run, part of the measured self-consistency disagreement is
sampling noise rather than genuine ambiguity**, so read `agreement` accordingly. Setting
`ANTHROPIC_API_KEY` moves the default judge off the CLI onto the bare Messages API (temperature pinned,
no ~40k-token auto-loaded context); without a key the CLI remains the path, for subscription users.

**The stamp covers generation *and* judging (D13).** Pinning only the judge was never enough: compare and
pairwise **generate** the candidate they grade, and a candidate redrawn on every run makes the run
irreproducible however deterministic the grading was. So candidate generation now goes through the same
pinned path as the judge (`temperature: 0` + the fixed seed, where the provider takes them), and the run
report carries **two facts instead of one**:

```json
"determinism": "best-effort",
"determinism_detail": { "generation": "best-effort", "judging": "exact" }
```

- `determinism` is the **weaker** of the two halves, so it can never overstate; `determinism_detail`
  names which half is the limit. A `null` half means that half did not happen — rubric and simple modes
  judge outputs the caller supplied and generate nothing.
- Ordering, weakest first: `sampled` < `best-effort` < `exact`.
- `sampled` is a *third*, deliberately-unpinned state: with `--gen-samples > 1` the operator is drawing a
  distribution of candidates on purpose (generation self-consistency). Pinning there would collapse every
  draw onto one output and silently delete the feature, so we sample — and say so — rather than claim
  reproducibility. A `--gen-samples 1` run over a seeded provider reads `exact` on both halves and does
  reproduce its candidates.
- No provider regresses: one with no sampling knobs (`claude -p`) still runs, degraded to `best-effort`,
  and is stamped as such rather than silently included in an `exact` claim.

**Dataset content pin.** `dataset_ref` pins an **id**, not content, and freezing is opt-in — so two runs
citing the same ref can have been scored on different cases. Every run over a referenced dataset now also
records `dataset_frozen` and `dataset_version` **as of run time**, and prints a note when the dataset is
not frozen. This records the truth; it deliberately does **not** change the policy — an unfrozen dataset
still runs, it just no longer *reads* as pinned.

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
- **Hub-side identity is derived, not asserted.** A hub does **not** trust the `contributor_id` in the
  request body (kept only for wire compat, ignored). It derives the identity from the presented bearer
  key — `c-` + first 12 hex of SHA-256(token) — so a poster can only ever replace *its own* set and can
  neither overwrite a victim nor forge ids to inflate `n_contributors`.
  - *Breaking change for keyless pushes.* A keyless (dev-mode) contribution is **refused** by default.
    Set `LIGHTTRACK_COLLECTIVE_ALLOW_ANON=1` to accept keyless pushes under one shared `anonymous`
    identity (with a loud warning) — but then all anonymous posters can overwrite each other, so prefer
    real keys. Contributors that previously pushed anonymously must now present a bearer key.
- **Hub-enforced k-floor.** The hub re-enforces its own `LIGHTTRACK_COLLECTIVE_MIN_CASES` (default 5,
  clamp ≥1): any contributed bucket with `n_cases` below it is dropped per-entry on ingest (not a whole
  request 400) and the count is returned as `dropped_under_min`, regardless of the floor the contributor
  claims it used.
- **Trustworthy merge math (digest schema v2).** A point estimate lies when a 5-case bucket ranks next
  to a 50k-case one, so v2 carries second-order summaries and the merge surfaces uncertainty:
  - *Per-bucket variance.* Each v2 entry adds `quality_variance` — the **case-weighted population
    variance of the contributing runs' mean scores** (`None` for a single-run bucket, variance
    undefined). A hub accepts **both v1 and v2** (`MIN_SCHEMA_VERSION..=DIGEST_SCHEMA_VERSION`); v1
    entries land with `quality_variance` NULL rather than being orphaned by the version bump.
  - *Approximate CI.* Each leaderboard row carries `quality ± quality_ci95` — a pooled, case-weighted
    95% interval (`SE = √(V/N_known)`, `V = Σnᵢvᵢ/Σnᵢ`). It is an honest **approximation** (it treats
    between-run variance as case-level dispersion and ignores between-contributor shifts, so it is a
    floor on the true uncertainty). When fewer than half the cases carry a known variance the CI is
    `None` — an explicit "insufficient variance data" marker, never a fabricated interval.
  - *Low-confidence, not hidden.* Rows aggregating fewer than the display floor
    (`LIGHTTRACK_COLLECTIVE_DISPLAY_FLOOR`, default 30) of cases are flagged `low_confidence` (shown,
    with a `†` in the rendered table) instead of being dropped. Ranking is always by the point estimate.
  - *Honest latency.* The merged `p50` is a case-weighted mean of contributors' per-run medians
    (approximate — labelled as such); `p95` is now surfaced as the **worst-observed** tail (the max
    across contributors), not silently discarded.
- **Judge context + model-identity normalization (v2).** Cross-instance quality is only commensurable
  when you know *how* it was scored and *which* model it is:
  - *Judge tagging.* Each v2 entry carries `judge_provider` (the coarse judge family
    `anthropic|openai|google|unknown` — provider only, never the full judge model, to limit
    fingerprinting) and `rubric_fingerprint` (a short one-way hash of the rubric shape — no content
    leak). The contributor derives both from its own run data (benchmark `judge_model` + rubric); the
    hub clamps the judge tag to the known vocabulary. A leaderboard row exposes the distinct
    `judge_providers` and, when they disagree, a `mixed_judges` count; `GET …/leaderboard?judge=<prov>`
    filters to rows a given judge family scored. A bucket whose own runs disagree collapses to `mixed`.
  - *Model-identity normalization.* At ingest the hub canonicalizes `(provider, model)` through
    `config/model_aliases.json` (`LIGHTTRACK_MODEL_ALIASES`): a redundant `provider/` prefix is stripped
    and dated/synonym variants collapse to their family **only where the alias file says so**
    (`gpt-4o`, `openai/gpt-4o`, `gpt-4o-2024-08-06` → one row). An identity absent from the table passes
    through unchanged, so a new model is never silently mis-merged.
- **Topology.** Any LightTrack can be a **hub** (`LIGHTTRACK_COLLECTIVE_ACCEPT=1`, off by default) that
  receives digests and merges them; others contribute. Same binary, no central service required.
- **API.** `GET /v1/collective/digest?min_cases=` (admin — preview what we'd publish) ·
  `POST /v1/collective/ingest` (hub-only; replaces a contributor's set, validates + clamps each entry) ·
  `GET /v1/collective/leaderboard?task_type=&provider=&judge=` (open read — the merged leaderboard).
- **Surfaces.** `lt collective leaderboard|digest|contribute --hub <url>` (the CLI does the two-hop push:
  GET own digest → POST to the hub); the `get_collective_leaderboard` MCP read tool; a rendered
  leaderboard table shared by CLI + MCP.

## Data model additions (SQLite ↔ Postgres ↔ Firestore, behind the Store trait)
This is what the schema actually contains — not a plan. Tables that were once aspirational here
(`targets`, `prompt_variants`, `case_results`) do **not** exist and are marked as such below.
```
datasets(id, project_id, name, version, frozen, source, created_at)
dataset_items(id, dataset_id, input, expected?, context?, tags, source_event_id?, anonymization)
-- NOT TABLES: the comparison matrix (provider × model × system_prompt variant) is stored inline as a
-- JSON array in benchmarks.target; there is no `targets` or `prompt_variants` table. Prompt variants
-- live in the prompt registry (prompts / prompt_versions).
benchmark_runs(... + p50_latency_ms, p95_latency_ms, total_tokens, cost_usd, report)
-- Case results are NOT a separate table: a case result IS a score row, run-scoped.
scores(id, project_id, event_id?, rubric, value, max, pass?, reasoning?, detail?,
       run_id?, case_index?, scored_by, cost_usd?, created_at)
       -- detail    = core::ScoreDetail as JSON: per-dimension {value, weight, floor, floor_hit,
       --             reasoning[]}, agreement, sample accounting, position-bias / injection flags,
       --             determinism. Bounded by ScoreDetail::capped() at the API boundary.
       -- run_id    = the benchmark run that produced this verdict (NULL for online/ad-hoc scores);
       --             case_index = its 1-based position in the run's dataset.
       -- Indexed by (run_id, case_index, created_at) → "every case result for run X" is one query:
       --   GET /v1/scores?run=<benchmark_run_id>
rubrics(id, project_id, name, dimensions_json, threshold)        -- weighted anchored dimensions;
                 -- each dimension: {key, description, weight, anchors?, floor?, kind?, check?}
model_prices(provider, model, input_per_mtok, output_per_mtok, cached_input_per_mtok, effective_date, source_url)
jobs(id, type, payload_json, status, attempts, progress, error, claimed_at, created_at)
collective_entries(contributor_id, provider, model, task_type, quality, pass_rate, avg_cost_usd,
                   p50_latency_ms?, p95_latency_ms?, n_runs, n_cases, quality_variance?,
                   judge_provider?, rubric_fingerprint?, received_at)  -- hub side; PK=(contributor_id,provider,model,task_type)
```

### Run-scoped case results
"Why did run 47 fail?" is a query, in **every** mode:

- The runner **mints the run id before judging** and stamps `run_id` + `case_index` on each verdict it
  posts. A case is therefore recorded even if the run row itself never lands — the opposite order
  would orphan a whole run's cases on one failed POST.
- Each mode posts one case result per unit of work: **simple** and **rubric** one per case,
  **compare** one per (target, case) — compare writes one run *per target*, so `case_index` is unique
  within a run — and **pairwise** one per game (`value` = A's outcome: win 1.0 / tie 0.5 / loss 0.0;
  `case_index` is the case the game was played on, so it repeats across that case's games).
- Every case carries its `ScoreDetail`: per-dimension values/floors, every sample's reasoning,
  agreement, sample accounting, position-bias / injection flags, determinism.
- Read them with `GET /v1/scores?run=<benchmark_run_id>` — ordered by `case_index`, then `created_at`,
  with unindexed cases last. Implemented on SQLite, Postgres and Firestore; a backend that has not
  ported it answers **501**, never an empty list (an empty list would read as "no failures").

**Inline report arrays are bounded and say so.** `cases` (simple, compare), `failing_cases` (rubric)
and `games` (pairwise) are previews of at most 200 entries, each accompanied by `<key>_total`,
`<key>_logged` and `<key>_truncated` (pairwise: `n_games` / `games_logged` / `games_truncated`). A
consumer can never mistake a clipped list for a complete one; the complete record is the run's scores.
Each mode also reports `score_post_failures` — cases whose verdict the API refused, so "the cases are
missing" is a recorded fact rather than something an operator has to infer.

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
