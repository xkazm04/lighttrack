# LightTrack — Data Model

All times are UTC. IDs are UUIDv4 strings unless noted. The same logical model backs SQLite (local) and
BigQuery (cloud); see `schema/`.

## `events` — one normalized LLM call
The heart of the system. Emitted by monitored apps, normalized + costed by `api`.

| Field | Type | Notes |
|---|---|---|
| `id` | string (uuid) | event id |
| `project_id` | string | FK → projects |
| `trace_id` | string? | groups multiple calls in one logical operation (OTel-aligned) |
| `span_id` | string? | this call's span |
| `parent_span_id` | string? | parent span, for nested agent calls |
| `ts` | timestamp | when the call happened |
| `provider` | string | `openai` \| `anthropic` \| `google` \| `unknown` |
| `model` | string | e.g. `gpt-4.1`, `claude-opus-4-8`, `gemini-2.5-pro` |
| `operation` | string | `chat` \| `completion` \| `embedding` \| `other` |
| `input_tokens` | int | |
| `output_tokens` | int | |
| `cached_input_tokens` | int? | billed at cached rate when priced |
| `reasoning_tokens` | int? | o-series / thinking |
| `cost_usd` | float? | provider-reported or computed from PriceBook |
| `latency_ms` | int? | |
| `status` | string | `success` \| `error` \| `timeout` |
| `error` | string? | message when status≠success |
| `input` | json? | messages/prompt — optional, redactable per project |
| `output` | json? | completion — optional, redactable |
| `tags` | json (array) | freeform labels |
| `source` | string? | host / app instance |
| `metadata` | json | arbitrary app-supplied fields |

## traces — a derived end-to-end view (no table)
A *trace* is every `event` sharing a `trace_id`, rolled up into one view of a multi-step / agentic
request. There is **no `traces` table**: the rollup is computed on read (`core::trace::Trace::from_events`)
from the events, and the span tree is reconstructed from `span_id` / `parent_span_id`. An event whose
parent is absent from the trace (or unset) is a root.

| Field | Type | Notes |
|---|---|---|
| `trace_id` | string | the shared id |
| `project_id` | string | from the trace's events |
| `started_at` / `ended_at` | timestamp | first / last event time |
| `duration_ms` | int | wall-clock span, `ended_at − started_at` |
| `status` | string | `error` if any span errored, else `success` |
| `totals` | object | `{spans, cost_usd, input_tokens, output_tokens, total_tokens, errors}` |
| `models` | string[] | distinct models touched, first-seen order |
| `spans` | tree | root `{event, children[]}` nodes (detail view only) |

Read via `GET /v1/traces` (compact rollups) and `GET /v1/traces/:id` (totals + span tree + scores
within the trace). A whole trace can be scored with `POST /v1/traces/:id/score`: the verdict is a
normal `scores` row anchored to the trace's root span event (or a named `event_id`), so it links back
through the same `event_id → trace_id` path the read side joins on — no per-score `trace_id` column.

## `projects`
| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `name` | string | |
| `enabled` | bool | |
| `redaction` | string | `none` \| `hash` \| `drop` — how to store prompts/outputs |
| `created_at` | timestamp | |

## `api_keys`
| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `project_id` | string | FK |
| `name` | string | label |
| `prefix` | string | non-secret display prefix, e.g. `lt_ab12cd` |
| `key_hash` | string | salted SHA-256 of the secret; raw key shown once at creation |
| `created_at` | timestamp | |
| `last_used_at` | timestamp? | |
| `revoked` | bool | |

## `limit_rules`
| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `project_id` | string | FK |
| `metric` | string | `cost_usd` \| `calls` \| `tokens` |
| `window` | string | `hour` \| `day` \| `month` |
| `threshold` | float | |
| `action` | string | `alert` (notify only) \| `throttle` \| `block` (both enforced at ingest: a breaching event is rejected with 429 and not recorded) |
| `enabled` | bool | |

## `scores` — LLM-as-judge results
| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `project_id` | string | FK |
| `event_id` | string? | scored event (null for benchmark-only) |
| `rubric` | string | rubric/metric name |
| `value` | float | |
| `max` | float | scale upper bound |
| `pass` | bool? | |
| `reasoning` | string? | judge rationale |
| `scored_by` | string | judge model, e.g. `claude-haiku-4-5` |
| `cost_usd` | float? | judge call cost (watched, never throttled) |
| `created_at` | timestamp | |

## `benchmarks` / `benchmark_runs`
| `benchmarks` | Type | | `benchmark_runs` | Type |
|---|---|---|---|---|
| `id` | string | | `id` | string |
| `project_id` | string | | `benchmark_id` | string |
| `name` | string | | `started_at` | timestamp |
| `rubric` | string | | `finished_at` | timestamp? |
| `judge_model` | string | | `n_cases` | int |
| `target` | json | | `mean_score` | float |
| `dataset_ref` | string | | `pass_rate` | float |
| `baseline_score` | float? | | `cost_usd` | float |
| `created_at` | timestamp | | `status` | string |

## Judge structured output (`--json-schema`)
`claude -p` returns this in `structured_output` (see `core::score::judge_verdict_schema`):
```json
{ "score": 0.0, "max": 1.0, "pass": true, "reasoning": "..." }
```
`api`/`runner` also read `total_cost_usd` and per-model `usage` from the `claude -p --output-format json`
envelope to populate `scores.cost_usd`.
