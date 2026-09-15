---
name: gateway-onboard
description: >-
  Move one of an app's LLM use cases onto the LightTrack gateway (`lt-gateway`): benchmark the two
  seat-metered engines (Claude via `claude -p`, GPT via the Codex CLI) on a difficulty-graded corpus
  built from that use case, pick the cheapest configuration of each that clears the quality bar on
  every tier, write the `gateway.toml` route with the other engine as the usage-limit fallback
  when it also clears the bar, switch the app's call site to the gateway, and prove the failover
  path end to end. Use when the user wants to "replace the LLM for <use case>", "onboard <app> to
  the gateway", "pick the model for <call site>", or "set up fallback between Claude and Codex".
  Invoke with `/gateway-onboard <app-path> <use-case-key>`.
---

# Gateway onboarding: one use case, two seats, a measured route

You are replacing an app's own LLM wrapper for **one use case** with a route on `lt-gateway`. The
route names a primary target and, when the measurement supports it, a fallback on the *other*
seat, so a usage limit on one subscription does not stop the app. Every decision below is read from
a benchmark run, not assumed. Read `docs/GATEWAY.md` (what the gateway does and does not do) and
`docs/BENCHMARK_FRAMEWORK.md` §0–§3 (difficulty ladder, targets, effort, judge rules) first.

Arguments: `<app-path>` (the consuming repo) and `<use-case-key>` (the `events.name` value; the
use-case registry key). Ask for either that is missing.

## Guardrails
- **Seats, not keys.** The two engines this flow measures are subscription-metered CLIs. Make sure
  `ANTHROPIC_API_KEY` is **unset** in the runner's environment, or the Anthropic path silently
  becomes the metered API (`anthropic_api::available()`), and the measurement is of a different
  product than the app will run on.
- **Nothing is written to the app until step 6.** Steps 1–5 only read the app and write to
  LightTrack (a dataset, a rubric, a benchmark). The user reviews the scorecard before any route
  exists.
- **Fewer targets, more cases.** Six targets is fifteen pairwise comparisons and a Bonferroni bar
  that refuses everything at n=18. Two or three per seat, and at least 8 cases per tier.
- **The difficulty ladder is for LLMs, not humans.** A "hard" tier that every model scores 100% on
  separates nothing and the operator paid for it. Scale the *input* (length, distractors,
  ambiguity, required structure), and re-grade after the first run if a tier reads flat.
- Judge cross-family. The matrix holds both families, so the judge cannot be neutral to both:
  prefer deterministic dimensions (`json_valid`, `regex`, `numeric`, `contains`) wherever the use
  case allows, use a third family (`google/...` with `GEMINI_API_KEY`, or `openrouter/...`) for
  the `llm` dimensions when a key exists, and otherwise keep the default judge and **report** the
  `self_preference` flag the run records beside every number that came from it.

## Step 0 — Preflight (report a short table, fix or stop)
- `lt-api` reachable (`GET $LIGHTTRACK_URL/health`), and the project id + key for this app
  (`LIGHTTRACK_PROJECT` / `LIGHTTRACK_KEY` in the app's env, or ask).
- `claude` authed on a seat: `claude -p "ping" --output-format json` answers with an envelope.
- `codex` current and authed: `codex exec --version`; on Windows two installs can coexist and the
  stale one refuses newer models — `LIGHTTRACK_CODEX_BIN` overrides.
- `lt-gateway` and `lt-runner` build: `cargo build -p lighttrack-gateway -p lighttrack-runner`.
- The app's current model for this use case, from its config or the events already in LightTrack
  (`GET /v1/usecases?project=` groups usage by name × provider × model).

## Step 1 — Read the call site, register the use case
Find the code that makes the call for `<use-case-key>`: the system prompt, how the input is
assembled, whether the output is parsed against a schema, and what "good" means to the code that
consumes it. Then register (idempotent upsert):

```
POST /v1/projects/:id/use-cases
{ "key": "<use-case-key>", "name": "...", "kind": "extraction|classification|summarization|generation|...",
  "component": "<file or module>", "expected_models": ["<current provider/model>"] }
```

`kind` decides the rubric shape in step 3. Note the schema, if any: it goes on every benchmark
target as `schema` (the engine's enforcement path — the same one the gateway uses for the app's
`response_format`) and drives the `json_valid` dimension. A schema only pasted into the system
prompt measures a transport the app will never ship.

## Step 2 — Build the corpus, graded
Prefer real traffic. If the use case has events in LightTrack:

```
lt-runner dataset build --project <id> --name <use-case-key>-bench --n 30 --strategy stratified
```

Otherwise craft cases from the app's fixtures. Either way, every case gets an explicit
`difficulty` of `easy`, `medium` or `hard` — never leave one ungraded, because an ungraded case
lands in no tier and the routing decision reads the tiers. Add `expected` wherever the answer is
mechanical (a label, a number, a required field value). Write items with:

```
POST /v1/projects/:id/datasets           { "name": "<use-case-key>-bench" }
POST /v1/datasets/:id/items              { "input": "...", "expected": "...", "difficulty": "hard", "tags": [...] }
POST /v1/datasets/:id/freeze
```

Aim for 8–12 cases per tier. Grade by what makes *this* use case hard for a model: longer inputs,
conflicting evidence, required output structure, edge values.

## Step 3 — The rubric
`POST /v1/projects/:id/rubrics`. Deterministic dimensions first, weighted highest:

- `json_valid` with `check.path` for a schema'd output (the shape the app parses).
- `exact` / `contains` / `numeric` against `expected` for classification and extraction.
- `regex` for a required format.
- One or two `llm` dimensions (`correctness`, `instruction_adherence`) with anchors, only where
  no mechanical check can say.

Set `threshold` to the bar the app actually needs (a parser that crashes on a missing field wants
1.0 on `json_valid`; use `gate` on that dimension).

## Step 4 — The benchmark: a ladder per seat
Targets are `{provider, model, effort, system_prompt}` rows; one identical `system_prompt` (the
app's) on every row so only `(model, effort)` varies. A first matrix of **four**:

```
POST /v1/projects/:id/benchmarks
{ "name": "<use-case-key> seats", "rubric_id": "<rubric>", "dataset_ref": "<dataset>",
  "judge_model": "<cross-family judge, see guardrails>",
  "targets": [
    { "provider": "anthropic", "model": "haiku",   "effort": "low",    "label": "haiku@low",   "system_prompt": "...", "schema": { ...the app's output schema... } },
    { "provider": "anthropic", "model": "sonnet",  "effort": "medium", "label": "sonnet@med",  "system_prompt": "..." },
    { "provider": "codex",     "model": "gpt-5.5", "effort": "low",    "label": "gpt-5.5@low", "system_prompt": "..." },
    { "provider": "codex",     "model": "gpt-5.5", "effort": "medium", "label": "gpt-5.5@med", "system_prompt": "..." }
  ] }
```

Run from the LightTrack repo root (the runner reads `.env` from the cwd). `--jobs` is a
top-level flag, before the subcommand:

```
lt-runner --jobs 3 bench --benchmark <id>
```

Escalate (`opus@high`, `gpt-5.6-*@high`) only if no row clears the hard tier; add rows to a
*second* benchmark rather than widening this one past six.

## Step 5 — Read the run and decide
`GET /v1/benchmarks/:id/runs` → the latest run's `report`. What to read, and the rule:

| In the report | Meaning | Rule |
|---|---|---|
| `targets[].mean`, `pass_rate`, `errored` | overall quality and reliability per row | a row with `errored > 0` on the seat path failed calls, not cases — read the error before trusting the mean |
| `targets[].tiers[] {tier, mean, n_cases}` | per-difficulty means | **the quality bar is the hard tier**: a row clears when its `hard` mean ≥ rubric `threshold` |
| `tier_discrimination.tiers[]` | whether a tier separated the rows | a flat tier (spread ≈ 0) says nothing; if `hard` is flat at the top, the corpus is too easy — regrade and rerun |
| `targets[].p50_latency_ms`, `cost_usd` | price of the row | Codex rows are **unpriced** (a seat reports no $); compare them on latency, and read Claude's CLI cost as the per-call floor plus tokens |
| `frontier` / `recommendation` | non-dominated rows, cheapest-sufficient | only over priced rows; treat as advisory here |
| `self_preference` | judge shares a family with a generator | say so beside every `llm`-dimension number |

Then, per seat: **primary candidate = the cheapest/fastest row of that seat that clears the hard
tier with `errored == 0`**. Pick the primary seat by hard-tier mean, breaking ties by latency.
The fallback is the other seat's candidate **if it clears the same bar**. If it clears `easy` and
`medium` only, it may still be the fallback — but write that on the route as a comment and in
`expected_models`, because during a limit window the app will run at that quality. If it clears
nothing, there is no fallback; say so plainly and stop after step 6.

Present the scorecard (rows × tiers, latency, errors, the judge caveat) and the proposed route to
the user with `AskUserQuestion` before writing anything into the app.

## Step 6 — Write the route and switch the app
`gateway.toml` in the app's repo (committable; no secrets):

```toml
[routes.<use-case-key>]
primary  = "anthropic/sonnet@medium"       # hard 0.93, p50 6.1s, 0 errors (run <run-id>)
fallback = ["codex/gpt-5.5@medium"]        # hard 0.90 — clears the bar; stands in on a Claude limit
```

The gateway refuses a fallback on the same seat as the primary at startup, by design. Update the
use case: `expected_models` = both targets.

Point the app at the gateway. The call site keeps its parsing and validation; only the transport
changes:

- **OpenAI SDK (any language):** `base_url = "http://127.0.0.1:8790/v1"`, `api_key = "lt"` (any
  non-empty string; the gateway does not read it), `model = "<use-case-key>"`. Structured output
  goes as `response_format: {type: "json_schema", json_schema: {schema}}`.
- **Plain HTTP:** `POST http://127.0.0.1:8790/v1/chat/completions` with the same body.
- Read `x-lighttrack-served-by` / `x-lighttrack-fell-back` (or `body.lighttrack`) if the app wants
  to log which seat answered. Do not branch on it.

Remove the app's own retry-and-fallback around this call: the gateway does it, and an app-side
retry on top would double every exhausted-seat attempt. Delete the app-side LightTrack `track()`
for this call site too — the gateway records every attempt itself, and two rows per call is a
double count.

Start it beside the app (from the app's repo, so `gateway.toml` and `.env` are found):

```
LIGHTTRACK_URL=... LIGHTTRACK_KEY=<project key> lt-gateway
```

## Step 7 — Prove it, then record it
With `LIGHTTRACK_GATEWAY_DEV=1` on the gateway (drills only — never in the normal run):

1. One real call through the app's code path. Expect `x-lighttrack-fell-back: 0`.
2. The same call with header `X-LightTrack-Simulate: exhausted:<primary provider>`. Expect
   `x-lighttrack-fell-back: 1`, `model` = the fallback, and `GET /health` on the gateway listing the
   primary under `cooldowns`.
3. A third call with no header: the primary is **skipped** (`attempts[0].outcome ==
   "skipped_cooling"`) and the fallback answers — this is the seamless resume.
4. `GET /v1/events?project=&name=<use-case-key>`: the drill left an `error` row on the primary
   (tag `provider_failed`, `metadata.failure_class = transient`) and a `success` row on the fallback
   (tag `fell_back`), sharing one `trace_id`.

Restart the gateway without the dev flag. Record the scorecard and the chosen route in the app's
docs (a `docs/LLM_ROUTES.md` table: use case, primary, fallback, hard-tier means, run id, date), and
commit `gateway.toml` with it. Leave the benchmark in place — it is the regression gate for the
next model change (`GET /v1/benchmarks/:id/gate`).

## When it does not go to plan
- **Every row at 100% on every tier** — the corpus is at the ceiling. Scale the hard tier's inputs
  and rerun before choosing; a choice made on a flat corpus is a coin flip with a scorecard.
- **The app's system prompt is over ~16k characters** — the Codex CLI takes instructions on the
  command line, so the engine folds a longer system prompt into the head of the user turn
  (announced on stderr). Codex rows then measure a folded shape while Claude rows take a system
  turn; say so beside the numbers, or fold the prompt on every target for a like-for-like matrix.
- **A seat row has `errored > 0`** — read the run's error text. A usage limit *during the
  benchmark* is a measurement gap, not a quality signal; rerun that row when the window resets.
  A model name the CLI refuses is a stale install or a wrong id.
- **The primary clears hard but the other seat clears nothing** — ship without a fallback and say
  the app stops on a limit. Do not lower the bar to manufacture one.
- **The use case is multi-turn or uses tools** — on the two seat CLIs the turns are rendered into
  one prompt (the response flags `transcript`) and tools are refused; only the OpenAI-shaped
  HTTP providers (`openai`, `openrouter`) take tools. A tool-using call site can be routed, but
  every target in its chain must be one of those, so it is a route between API keys rather than
  seats — say so, and benchmark those targets instead.
