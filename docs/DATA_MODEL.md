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
| `metadata` | json | arbitrary app-supplied fields, **plus the server-owned keys below** |

#### Server-owned `metadata` keys
Written by the server and stripped from whatever the client sent, so they can be trusted as
attribution and provenance rather than as claims: `api_key_id` (the authenticated principal),
`cost_source` / `pricing_mode` (how `cost_usd` was resolved), and `redaction` — the
[`RedactionStamp`](../crates/core/src/project.rs) `{policy, scrub, spans, rules}` recording what the
ingest boundary did to this row. `rules` is `lighttrack_anon::rules_fingerprint()`, the digest of the
scrubber's ordered rule set, so a span count written before a rule change is never silently compared
with one written after. `customer_id` / `product_id` are client-supplied but pass the PII scrub
un-rewritten (they are join keys, not payloads).

A row with **no** `redaction` key predates the stamp or was written by a path that does not scrub —
which is a different finding from `scrub: false` (the boundary looked and stored the text verbatim),
and `GET /v1/projects/:id/redaction` reports the two separately rather than folding them together.

### Querying `events`
`GET /v1/events` AND-combines: `project`, `since`/`until` (client `ts`), `provider`, `model`,
`trace_id`, `name`, `status` (`success|error|timeout`), `tag` (array **membership**, not substring),
`meta` (`key` or `key=value` over `metadata` — how per-customer/product questions are asked, since that
linkage rides in metadata rather than a column), `min_cost`, `redaction_rules` (rows stamped by one
scrubber rule set) and `min_redacted_spans` (rows the scrub actually rewrote). `count=1` additionally returns
`X-Total-Count`: the size of the whole matching set, independent of the cursor and page limit, so a
client can render "n of N" without paging to count. Paging is keyset (`X-Next-Cursor`) and the cursor
predicate is independent of the content predicates, so traversal is exact under every filter
combination.

Composite indexes `(project_id, provider|model|status, ts)` serve each equality **and** the
`ORDER BY ts DESC` in one seek; `min_cost`, `tag` and `meta` are residual within that range. Backends
that have not ported the extended predicates answer **501 `unsupported`** naming the filter — never an
unfiltered page presented as if the filter had been honored.

### Rolling `events` up — the dimension vocabulary

One primitive answers every "usage and cost over a window, grouped by X" question:
`Store::rollup(RollupQuery)` (`crates/core/src/rollup.rs`), surfaced as
`GET /v1/rollup?by=…&since=…&until=…&time=…&filter=…`. `/v1/costs`, `/v1/costs/prompts`,
`/v1/usecases`, `/v1/limits/usage`, `/v1/margin/*` and `/v1/forecast` are all fixed groupings of it,
and every backend implements the primitive once rather than one query per surface.

`Dimension` is the **single** vocabulary — limit scopes (`LimitScope::kind_str`), the legacy `dim`
arguments and the SQL/JSON extraction all route through it, so a dimension cannot exist in one place
and silently not in another.

| Dimension  | Where it lives on the row              | Also a limit scope? |
|------------|----------------------------------------|:-------------------:|
| `project`  | `project_id` column                    |                     |
| `provider` | `provider` column                      | yes                 |
| `model`    | `model` column                         | yes                 |
| `name`     | `name` column (the use-case label)     | yes                 |
| `api_key`  | `metadata.api_key_id` (server-stamped) | yes                 |
| `customer` | `metadata.customer_id`                 | yes                 |
| `product`  | `metadata.product_id`                  |                     |
| `prompt`   | `metadata.prompt` (`name@vN`)          |                     |
| `day`      | UTC day of the query's time key        |                     |

A rollup groups by 1..=3 distinct dimensions; a row's `keys` align positionally with `group_by`. A
row that carries no value on a dimension folds into a single `null` bucket rather than being dropped,
so the parts always sum to the whole. Filters are equality only, and a `null` value never matches one
(an untagged call cannot be charged to a customer).

`time` selects the window and `day` bucket key: `ts` (client-declared) or `received_at` (server
arrival). **Accounting reads use `received_at`** — a caller able to slide its spend out of a window by
backdating its own events is a caller with no cap. Firestore stores no `received_at` and answers both
on `ts`; see `docs/PARITY.md`.

`api_key` is admin-only in both `by` and `filter`: grouping on it enumerates a project's key ids.

### The `unpriced_calls` disclosure rule

`cost_usd` is the **stored** sum — what `SUM(cost_usd)` sees. A call whose model was absent from the
price book has `cost_usd IS NULL` on the row (we never write a phantom zero), so it contributes
nothing. Every rollup row therefore also carries `unpriced_calls`, and `CostRow`, `UseCaseCostRow` and
`CostByDimension` carry it too.

The rule for anything that displays a cost: **when `unpriced_calls > 0`, the number is a floor, not a
total, and must be presented as one.** A zero indistinguishable from "we don't know" is the failure
this field exists to remove — it is why the limit path charges unpriced traffic by imputation
(`Usage::cost_for_limits`) instead of letting an unpriced model be spent for free, and why the
rendered rollup table prints the caveat under the total.

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
| `rubric` | string | the verdict's human-readable **label** — six encodings live here (a rubric name, `bench:{name}`, `{name}:{label}#case{i}`, a pairwise pairing, `lt:calibration:…`), which is why it is no longer the identity |
| `rubric_id` | string? | the `rubrics` row this was judged against — the join the label could never be: stable across a rename, unique across two rubrics that share a name |
| `kind` | string? | `freeform` \| `rubric` \| `bench_case` \| `compare_cell` \| `pairwise_game` \| `calibration` \| `trace`. Absent reads as `freeform` (the pre-typing default); an unrecognized value reads as `other` rather than being misfiled |
| `value` | float | |
| `max` | float | scale upper bound |
| `pass` | bool? | |
| `reasoning` | string? | judge rationale |
| `scored_by` | string | judge model, e.g. `claude-haiku-4-5` |
| `cost_usd` | float? | judge call cost (watched, never throttled) |
| `created_at` | timestamp | |

`GET /v1/scores` narrows on `rubric_id` and `kind` — a benchmark case is not the same measurement as
an ad-hoc verdict, and averaging them together is what those filters exist to prevent. The alerting
window keys on the same identity (`Score::alert_key`): per-case verdicts roll up under their
benchmark, because a label carrying `#case7` is unique per case and a window that never sees the same
key twice can never accumulate.

## `rubrics`
Beyond `{id, project_id, name, dimensions, threshold, created_at}`:

| Field | Type | Notes |
|---|---|---|
| `version` | int | generation, from 1. Omitted from the wire at 1 (absent means 1) |
| `supersedes` | string? | the rubric id this one replaces |

A new version is a **new row with a new id** (`POST /v1/rubrics/:id/versions`), never a mutation:
verdicts already stored cite the old rubric's id, and rewriting that row would silently change what
those verdicts claim to have measured. The superseded rubric stays readable and stays cited. The
collective digest's `rubric_fingerprint` includes the version for the same reason — two runs judged
under materially different criteria must not merge into one leaderboard bucket.

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

<!-- BEGIN generated table index (cargo test -p lighttrack-store --test schema_doc) -->

## Every table (generated)

Rendered from the declarative model in `crates/store/src/schema/tables/`, which is also what generates the three DDLs in `schema/`. A table here without a section above is one this document has not explained yet — which is the point of generating the list.

| Table | Columns | Added after ship | Indexes | Key |
|---|---|---|---|---|
| `projects` | 8 | 3 | 0 | `id` |
| `api_keys` | 10 | 2 | 1 | `id` |
| `events` | 24 | 2 | 8 | `id` |
| `limit_rules` | 15 | 8 | 0 | `id` |
| `scores` | 16 | 5 | 6 | `id` |
| `benchmarks` | 11 | 0 | 0 | `id` |
| `rubrics` | 8 | 2 | 0 | `id` |
| `jobs` | 15 | 3 | 2 | `id` |
| `prompts` | 9 | 2 | 1 | `id` |
| `prompt_versions` | 7 | 0 | 1 | `id` |
| `benchmark_runs` | 13 | 0 | 0 | `id` |
| `model_prices` | 9 | 0 | 0 | `provider, model, effective_from` |
| `datasets` | 8 | 1 | 1 | `id` |
| `dataset_items` | 10 | 1 | 2 | `id` |
| `revenue_events` | 16 | 4 | 2 | `id` |
| `collective_entries` | 18 | 6 | 2 | `contributor_id, provider, model, task_type` |
| `relay_tasks` | 21 | 4 | 3 | `id` |
| `margin_policies` | 8 | 0 | 1 | `id` |
| `schedules` | 9 | 0 | 2 | `id` |
| `devices` | 10 | 0 | 2 | `id` |
| `alerts` | 11 | 0 | 3 | `id` |
| `alert_channels` | 10 | 0 | 1 | `id` |
| `collective_contributions` | 12 | 0 | 2 | `id` |
| `labels` | 11 | 0 | 3 | `id` |
| `calibrations` | 14 | 0 | 1 | `id` |

Totals: **25 tables**, **303 columns**, of which **43** were added after their table shipped (those are `ALTER TABLE … ADD COLUMN` on every dialect, never edits to a `CREATE TABLE`). Schema fingerprint: `sha256-d329cc1689cdc517` — the same value `GET /v1/capabilities` reports.

<!-- END generated table index -->
