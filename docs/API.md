# API surface

<!-- GENERATED FILE — do not edit. Source: `crates/contract/src/endpoints/`.
     Regenerate: `LIGHTTRACK_WRITE_API_MATRIX=1 cargo test -p lighttrack-contract matrix` -->

Every HTTP endpoint this deployment serves, who may call it, and which of the three client
surfaces reaches it. The axum router, the MCP tool catalog, the `lt` verb tree and the
Markdown renderer are all generated from or held to the same table, so this document cannot
describe a route that does not exist, or miss one that does.

| | |
|---|---|
| Endpoints (method × path) | 125 |
| Distinct `/v1` routes | 99 |
| MCP tools | 64 (43 read, 21 write) |
| CLI verbs | 88 |
| Endpoints with a Markdown renderer | 46 |
| Machine doors (SDK / device / provider) | 15 |
| Paged reads | 6 |
| Reachable from neither MCP nor CLI | 0 |

## Endpoints

`Scope` is the capability a **project key** needs; `admin` means no project key reaches it
whatever its scopes, and `—` means the door authenticates something that is not a LightTrack
principal at all. A blank MCP or CLI cell is not an oversight where the row is marked 🔒:
those are machine doors (an SDK's ingest, a device agent's lease, a provider's webhook).

| Method | Path | Scope | MCP tool | CLI | Renderer |
|---|---|---|---|---|---|
| GET | `/health` | — |  | `lt health` |  |
| GET | `/openapi.json` | — |  | `lt openapi` |  |
| GET | `/v1/capabilities` | `read` | `get_capabilities` | `lt capabilities` | `get_capabilities` |
| POST | `/v1/events` | `ingest` | 🔒 |  |  |
| GET | `/v1/events` | `read` | `query_events` | `lt events` | `query_events` |
| POST | `/v1/events/batch` | `ingest` | 🔒 |  |  |
| GET | `/v1/ingest/status` | admin |  | `lt ingest status` |  |
| GET | `/v1/storage/status` | admin |  | `lt storage status` |  |
| GET | `/v1/events/:id` | `read` | `get_event` |  | `get_event` |
| POST | `/v1/traces` | `ingest` | 🔒 |  |  |
| GET | `/v1/traces` | `read` | `list_traces` | `lt traces` | `list_traces` |
| GET | `/v1/traces/:id` | `read` | `get_trace` | `lt trace` | `get_trace` |
| POST | `/v1/traces/:id/score` | `ingest` | `score_trace` |  |  |
| GET | `/v1/costs` | `read` | `get_cost_summary` | `lt costs` | `get_cost_summary` |
| GET | `/v1/costs/prompts` | `read` |  | `lt costs prompts` |  |
| GET | `/v1/costs/unpriced` | `read` | `list_unpriced_models` | `lt prices unpriced` | `list_unpriced_models` |
| GET | `/v1/quality/prompts` | `read` | `get_prompt_quality` | `lt prompts quality` | `get_prompt_quality` |
| GET | `/v1/usecases` | `read` | `get_usecases` |  | `get_usecases` |
| GET | `/v1/rollup` | `read` | `query_rollup` | `lt rollup` | `query_rollup` |
| GET | `/v1/prices` | `read` | `list_prices` | `lt prices list` | `list_prices` |
| GET | `/v1/prices/history/:provider/:model` | `read` | `list_price_history` | `lt prices history` | `list_price_history` |
| PUT | `/v1/prices/:provider/:model` | admin | `put_price` |  |  |
| GET | `/v1/forecast` | `read` | `get_forecast` | `lt forecast` | `get_forecast` |
| POST | `/v1/scores` | `ingest` | `record_score` |  |  |
| GET | `/v1/scores` | `read` | `list_scores` |  | `list_scores` |
| POST | `/v1/projects/:id/datasets` | admin | `create_dataset` |  |  |
| GET | `/v1/projects/:id/datasets` | `read` | `list_datasets` |  | `list_datasets` |
| GET | `/v1/datasets/:id` | `read` | `get_dataset` |  | `get_dataset` |
| POST | `/v1/datasets/:id/items` | admin | `add_dataset_item` |  |  |
| GET | `/v1/datasets/:id/items` | `read` | `list_dataset_items` |  | `list_dataset_items` |
| POST | `/v1/datasets/:id/freeze` | admin | `freeze_dataset` |  |  |
| POST | `/v1/datasets/:id/items/from-label` | admin |  | `lt datasets promote` |  |
| GET | `/v1/datasets/:id/labels` | `read` |  | `lt datasets labels` |  |
| POST | `/v1/datasets/:id/fork` | admin | `fork_dataset` | `lt datasets fork` |  |
| POST | `/v1/datasets/:id/items/import` | admin | `import_dataset_items` | `lt datasets import` |  |
| GET | `/v1/projects/:id/datasets/versions` | `read` |  | `lt datasets versions` |  |
| POST | `/v1/labels` | `manage` | `record_label` | `lt labels add` |  |
| GET | `/v1/labels` | `read` | `list_labels` | `lt labels list` | `list_labels` |
| POST | `/v1/calibrations` | `manage` |  | `lt judges calibrate` |  |
| GET | `/v1/calibrations` | `read` | `list_calibrations` | `lt judges history` | `list_calibrations` |
| GET | `/v1/judges/trust` | `read` | `get_judge_trust` | `lt judges trust` | `get_judge_trust` |
| POST | `/v1/projects/:id/rubrics` | admin | `create_rubric` | `lt rubrics create` |  |
| GET | `/v1/projects/:id/rubrics` | `read` | `list_rubrics` | `lt rubrics list` | `list_rubrics` |
| GET | `/v1/rubrics/:id` | `read` | `get_rubric` | `lt rubrics show` | `get_rubric` |
| POST | `/v1/rubrics/:id/versions` | admin |  | `lt rubrics version` |  |
| POST | `/v1/projects/:id/benchmarks` | admin | `create_benchmark` |  |  |
| GET | `/v1/projects/:id/benchmarks` | `read` | `list_benchmarks` |  | `list_benchmarks` |
| GET | `/v1/benchmarks/:id` | `read` | `get_benchmark` |  | `get_benchmark` |
| GET | `/v1/benchmarks/:id/runs` | `read` | `get_benchmark_runs` |  | `get_benchmark_runs` |
| GET | `/v1/benchmarks/:id/gate` | `read` | `check_benchmark_gate` |  | `check_benchmark_gate` |
| POST | `/v1/benchmark-runs` | `manage` | 🔒 |  |  |
| POST | `/v1/benchmarks/:id/enqueue` | admin | `enqueue_benchmark` |  |  |
| POST | `/v1/projects/:id/prompts` | admin |  | `lt prompts create` |  |
| GET | `/v1/projects/:id/prompts` | `read` | `list_prompts` | `lt prompts list` | `list_prompts` |
| GET | `/v1/projects/:id/prompts/:name` | `read` | `get_prompt` |  | `get_prompt` |
| PUT | `/v1/projects/:id/prompts/:name` | admin |  | `lt prompts link` |  |
| POST | `/v1/projects/:id/prompts/:name/versions` | admin | `create_prompt_version` |  |  |
| GET | `/v1/projects/:id/prompts/:name/versions` | `read` |  | `lt prompts versions` |  |
| PUT | `/v1/projects/:id/prompts/:name/canary` | admin |  | `lt prompts canary` |  |
| POST | `/v1/projects/:id/prompts/:name/promote` | admin | `promote_prompt` |  |  |
| GET | `/v1/jobs` | admin | `list_jobs` | `lt jobs list` | `list_jobs` |
| POST | `/v1/jobs` | admin | `enqueue_job` | `lt jobs enqueue` | `get_job` |
| POST | `/v1/jobs/claim` | admin | 🔒 |  |  |
| GET | `/v1/jobs/:id` | admin | `get_job` | `lt jobs show` | `get_job` |
| POST | `/v1/jobs/:id/cancel` | admin |  | `lt jobs cancel` |  |
| POST | `/v1/jobs/:id/progress` | admin | 🔒 |  |  |
| POST | `/v1/jobs/:id/renew` | admin | 🔒 |  |  |
| POST | `/v1/jobs/:id/finish` | admin | 🔒 |  |  |
| POST | `/v1/projects/:id/schedules` | admin | `create_schedule` | `lt schedules create` |  |
| GET | `/v1/projects/:id/schedules` | `read` |  | `lt schedules list` | `list_schedules` |
| GET | `/v1/schedules` | admin | `list_schedules` | `lt schedules list` | `list_schedules` |
| PUT | `/v1/schedules/:id` | admin |  | `lt schedules set` |  |
| DELETE | `/v1/schedules/:id` | admin |  | `lt schedules delete` |  |
| GET | `/v1/schedules/:id/runs` | admin |  | `lt schedules runs` | `list_jobs` |
| POST | `/v1/projects` | admin | `create_project` | `lt projects create` |  |
| GET | `/v1/projects` | admin | `list_projects` | `lt projects list` | `list_projects` |
| PUT | `/v1/projects/:id` | admin |  | `lt projects update` |  |
| DELETE | `/v1/projects/:id` | admin |  | `lt projects archive` |  |
| GET | `/v1/projects/:id/redaction` | `read` |  | `lt projects redaction` |  |
| POST | `/v1/projects/:id/keys` | admin |  | `lt keys create` |  |
| GET | `/v1/projects/:id/keys` | admin |  | `lt keys list` |  |
| DELETE | `/v1/projects/:id/keys/:kid` | admin |  | `lt keys revoke` |  |
| POST | `/v1/projects/:id/keys/:kid/rotate` | admin |  | `lt keys rotate` |  |
| POST | `/v1/projects/:id/limits` | admin | `create_limit` | `lt limits set` |  |
| GET | `/v1/projects/:id/limits` | `read` | `list_limits` | `lt limits list` | `list_limits` |
| PUT | `/v1/limits/:id` | admin | `update_limit` | `lt limits update` |  |
| DELETE | `/v1/limits/:id` | admin | `delete_limit` | `lt limits delete` |  |
| POST | `/v1/projects/:id/margin-policies` | admin |  | `lt margin-policies create` |  |
| GET | `/v1/projects/:id/margin-policies` | admin | `list_margin_policies` | `lt margin-policies list` | `list_margin_policies` |
| DELETE | `/v1/projects/:id/margin-policies/:pid` | admin |  | `lt margin-policies delete` |  |
| GET | `/v1/limits/status` | `read` | `get_limit_status` | `lt limits status` | `get_limit_status` |
| GET | `/v1/limits/usage` | `read` |  | `lt limits usage` |  |
| POST | `/v1/relay/tasks` | `ingest` |  | `lt relay tasks enqueue` |  |
| GET | `/v1/relay/tasks` | `read` | `list_relay_tasks` | `lt relay tasks list` |  |
| GET | `/v1/relay/tasks/:id` | `read` | `get_relay_task` |  |  |
| POST | `/v1/relay/tasks/:id/result` | admin | 🔒 |  |  |
| POST | `/v1/relay/tasks/:id/renew` | admin | 🔒 |  |  |
| POST | `/v1/relay/tasks/:id/progress` | admin | 🔒 |  |  |
| POST | `/v1/relay/tasks/:id/cancel` | `manage` |  | `lt relay tasks cancel` |  |
| POST | `/v1/relay/lease` | admin | 🔒 |  |  |
| POST | `/v1/relay/devices` | admin |  | `lt relay devices add` |  |
| GET | `/v1/relay/devices` | admin | `list_relay_devices` | `lt relay devices list` |  |
| DELETE | `/v1/relay/devices/:id` | admin |  | `lt relay devices revoke` |  |
| GET | `/v1/relay/actions` | `read` |  | `lt relay actions` |  |
| POST | `/v1/relay/actions/:action_type/dataset` | admin |  | `lt relay actions snapshot` |  |
| POST | `/v1/revenue` | admin |  | `lt revenue record` |  |
| POST | `/v1/revenue/reprice` | admin |  | `lt reprice` |  |
| GET | `/v1/margin` | admin | `get_margin` | `lt margin` | `get_margin` |
| GET | `/v1/margin/trend` | admin |  | `lt margin trend` | `get_margin_trend` |
| GET | `/v1/margin/customer/:id` | admin |  | `lt margin customer` | `get_margin_customer` |
| GET | `/v1/margin/simulate` | admin |  | `lt margin simulate` | `get_margin_simulate` |
| POST | `/v1/billing/:provider/webhook` | — | 🔒 |  |  |
| GET | `/v1/collective/digest` | admin | `get_collective_digest` | `lt collective digest` | `get_collective_digest` |
| POST | `/v1/collective/ingest` | `ingest` | 🔒 |  |  |
| GET | `/v1/collective/leaderboard` | `read` | `get_collective_leaderboard` | `lt collective leaderboard` | `get_collective_leaderboard` |
| DELETE | `/v1/collective/contribution` | `ingest` |  | `lt collective withdraw` |  |
| POST | `/v1/collective/contribute` | admin |  | `lt collective contribute` |  |
| GET | `/v1/collective/contributions` | admin | `get_collective_contributions` | `lt collective history` | `get_collective_contributions` |
| GET | `/v1/alerts` | `read` | `list_alerts` | `lt alerts list` | `list_alerts` |
| POST | `/v1/alerts/:id/ack` | `manage` | `ack_alert` | `lt alerts ack` |  |
| POST | `/v1/alerts/:id/resolution` | admin | 🔒 |  |  |
| GET | `/v1/projects/:id/alert-channels` | admin |  | `lt alerts channels list` |  |
| PUT | `/v1/projects/:id/alert-channels` | admin |  | `lt alerts channels set` |  |
| DELETE | `/v1/projects/:id/alert-channels/:cid` | admin |  | `lt alerts channels delete` |  |
| POST | `/v1/alert-channels/:id/test` | admin |  | `lt alerts channels test` |  |

## Response types

61 of 125 endpoints return a named type that derives `schemars::JsonSchema`, so
`/openapi.json` describes their fields. The other 63 build their body with
`serde_json::json!` and have no struct to point at; the contract describes each in prose and
the generated document carries that prose instead of a field list. Turning one into a named
type is a strict improvement that needs no coordination — add the struct, derive
`JsonSchema`, bind it in `crates/api/src/schema_registry.rs`, and point the row at it.
