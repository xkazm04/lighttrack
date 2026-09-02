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
| `ts` | timestamp | when the call happened, **as the client reports it**. Queryable/orderable; drives `since`/`until` on `GET /v1/events`, traces, and the cost/use-case rollups. Rejected on ingest when it is further than the configured skew window from server time (`ts_too_old` / `ts_too_new`, HTTP 400). |
| `received_at` | timestamp | when the API accepted the call — **server-stamped, never read from the request body**. Every rolling-window accounting read keys on this: limit admission, `GET /v1/limits/status`, and the daily forecast series. That split is deliberate: a client owns its `ts`, so if budgets were measured on it a single wrong clock would silently corrupt enforcement. Rows written before this column existed are backfilled to `received_at = ts`. |
| `provider` | string | **Open vocabulary** (M8): any vendor id, canonicalized to lowercase `[a-z0-9._-]` (`openai`, `anthropic`, `google`, `mistral`, `groq`, `az.ai.openai`, …). `unknown` is the sentinel for "no provider was reported" — and, for rows written **before** M8, the backfill value every vendor outside the old closed enum was stored as; those rows cannot be recovered, so a `provider=unknown` row is either genuinely untagged or pre-M8 unmodeled traffic. Prices, limit scopes and rollups key on the raw id, so a `PUT /v1/prices/mistral/<model>` row is reachable by a `mistral` event. Dashboards that hard-code the three literals will now see more values; group by the column, don't enumerate it. |
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

### Querying `events`
`GET /v1/events` AND-combines: `project`, `since`/`until` (client `ts`), `provider`, `model`,
`trace_id`, `name`, `status` (`success|error|timeout`), `tag` (array **membership**, not substring),
`meta` (`key` or `key=value` over `metadata` — how per-customer/product questions are asked, since that
linkage rides in metadata rather than a column) and `min_cost`. `count=1` additionally returns
`X-Total-Count`: the size of the whole matching set, independent of the cursor and page limit, so a
client can render "n of N" without paging to count. Paging is keyset (`X-Next-Cursor`) and the cursor
predicate is independent of the content predicates, so traversal is exact under every filter
combination.

Composite indexes `(project_id, provider|model|status, ts)` serve each equality **and** the
`ORDER BY ts DESC` in one seek; `min_cost`, `tag` and `meta` are residual within that range. Backends
that have not ported the extended predicates answer **501 `unsupported`** naming the filter — never an
unfiltered page presented as if the filter had been honored.

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
| `duration_ms` | int | wall-clock span from the first span's start to the last span's **finish** — `max(ts + latency) − started_at`, so a trailing call's compute time counts. It may exceed `ended_at − started_at` (start-to-start). One definition (`core::trace::TraceShape`), so the list and the detail view always report the same number |
| `status` | string | `error` if any span errored, else `success` — same `TraceShape` rule on both views |
| `totals` | object | `{spans, cost_usd, input_tokens, output_tokens, total_tokens, errors}` |
| `models` | string[] | distinct models touched, first-seen order |
| `spans_total` / `spans_logged` / `spans_truncated` | int / int / bool | detail view only. The detail read is capped at `MAX_TRACE_SPANS` (5 000) spans, oldest first; when the cap bites, `spans_truncated` is true and every derived number (`totals`, `models`, `duration_ms`, `status`) covers the retained spans only |
| `spans` | tree | root `{event, children[]}` nodes (detail view only). A node carries `duplicate_span_id` when an earlier span already claimed its `span_id` — both are distinct calls, only the first owns the id for parent linkage |

Read via `GET /v1/traces` (compact rollups) and `GET /v1/traces/:id` (totals + span tree + scores
within the trace). A whole trace can be scored with `POST /v1/traces/:id/score`: the verdict is a
normal `scores` row anchored to the trace's root span event (or a named `event_id`), so it links back
through the same `event_id → trace_id` path the read side joins on — no per-score `trace_id` column.

**Project scope is part of the query.** A `trace_id` is caller-supplied and therefore not a tenant
boundary — two projects can both pick `req-1`. Every trace read (listing, detail, the trace's scores,
and the models on a summary) filters on `project_id` in SQL, so a colliding id in another project is
*invisible* rather than merged and then authorized away; asking for someone else's trace is a **404**,
not a 403. **Backend support:** SQLite and Postgres serve the full surface (same semantics, asserted
by the shared conformance suite — grouping, filters, the `(ended, trace_id)` keyset, the span cap and
its truncation signal, and the one `TraceShape` duration rule). Firestore has no server-side grouping
by `trace_id` and refuses with **501 `unsupported`** (see `docs/FIRESTORE.md`) — never an empty page.
`Store::serves_traces()` is the capability flag the suite branches on.

## `projects`
| Field | Type | Notes |
|---|---|---|
| `id` | string | caller-choosable at create; else a server-minted UUID (see below) |
| `name` | string | |
| `enabled` | bool | `false` refuses the project's events on both ingest doors (single POST → 403 `forbidden`; batch item → `invalid`/`forbidden`), nothing stored; reads and the project's keys keep working. Takes effect on the next event via the ingest policy cache |
| `redaction` | string | `none` \| `hash` \| `drop` — how to store prompts/outputs |
| `created_at` | timestamp | immutable |

`POST /v1/projects` accepts an optional `id` and uses it verbatim; omit it and the server mints a
UUID. A supplied id must be 1–64 characters, first an ASCII letter or digit, then letters, digits,
`-`, `_` or `.` — the alphabet that is unambiguous in a URL path segment, a `?project=` value, a
`LIGHTTRACK_PROJECT` env var and a Firestore document id at once. A malformed id is a `400` naming
the rule; an id already taken is a `409`. It is never silently replaced, because the id is what the
caller must type into the very next request (`POST /v1/projects/<id>/keys`) — the dev-mode bootstrap
already creates a readable `default` project, and this is the same privilege for everyone else.

Mutable fields are editable via `PUT /v1/projects/:id` (admin; omitted fields are left as-is). The
ingest path caches `redaction` per project so it doesn't pay a store read per event; because that field
is a *compliance* control, the cache has two freshness guarantees: an update through this API
invalidates the entry, so a tightening binds the **next** event with no restart, and every entry expires
after `LIGHTTRACK_REDACTION_CACHE_TTL_SECS` (default 60; `0` disables caching), which bounds staleness
for changes made by another replica or directly in the DB.

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
