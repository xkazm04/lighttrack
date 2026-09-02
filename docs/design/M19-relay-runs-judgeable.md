# M19 — Relay runs become judgeable events; actions become fingerprinted prompts

Size L · gate policy (opt-in I/O at rest in the cloud) · wave D · contexts: relay-queue,
device-agent, scoring, trace-analytics, dataset-management · depends on M18 (devices), M6 (ActionSpec),
M7 (lease/settle)

## Problem
LightTrack is an LLM-as-judge product, yet the one LLM workload it *originates* is unscoreable by
construction: the relay run event is written with `input: None, output: None`
(`crates/api/src/relay_result.rs` after M7's split — previously `relay.rs` ~349-350), and both
judges skip events without content (`runner/score.rs` "no content", `score_traces.rs` "root has no
output"). The result JSON sits in `relay_tasks.result`, read by no judge. The action's `prompt.md`
is edited in place on disk with no version, fingerprint or change note (`agent/actions.rs`), while the
prompt registry versions app prompts with benchmark-gated promotion. An action prompt can regress
silently for months.

## Design
1. `ActionSpec` (M6 already added mode/workspace/tools/budget): `+= version: Option<String>, report_io: bool (default false)`.
   `exec.rs` computes `prompt_sha256` over the **rendered** prompt and, only when `report_io`,
   includes `rendered_prompt` and `result_text` in `RunReport`; `cloud.rs::settle` forwards them.
2. `ResultReq` (`api/relay_result.rs`): `+= prompt_sha256, action_version, input: Option<Value>, output: Option<Value>`.
   `relay_run_event` fills `input`/`output` from them and stamps `metadata.prompt_sha256`,
   `metadata.action_version`, `metadata.action_type`; the payload goes through the same redaction
   door the error string already uses (`redact_event` — extend its scope to `input`/`output` here if
   the current call only scrubs `error`; M9's redaction stamp then lands on relay rows too).
3. Judges: no new scorer. Confirm `score`/`score-traces` pick relay events up once content exists
   (test with a synthetic relay event, `name = "relay-run"`, tag `relay`). Auto-score policy: allow a
   rule keyed on `tag = relay` / `metadata.action_type` so each action gets its rubric — read the
   shipped trace-auto-score-policy selector in `api/traces.rs` first and extend its filter shape,
   do not fork it.
4. `POST /v1/relay/actions/:action_type/dataset` (admin): snapshot succeeded tasks'
   `(payload → input, result → output)` into a dataset (reuses `datasets.rs` create + items), so a
   benchmark can be linked and an action's prompt change becomes benchmark-gated like a registry
   prompt. `GET /v1/relay/actions` (READ): distinct `action_type × prompt_sha256 × first/last seen`
   **derived from events** — no new table. Store: `list_relay_tasks` gains an `action_type` filter
   (SQLite + PG; Firestore `Unsupported`) if needed. Routes in `ROUTE_SCOPES`.
5. `docs/RELAY.md` "Cost model" gains a "Quality model" section; `actions/README.md` documents
   `version` and `report_io` and the privacy default (off; the cloud holds the fingerprint only
   unless the action opts in).

## Out of scope
Devices/capabilities (M18, merged). Cost pricing / enqueue admission (M5, same wave — do not touch
`relay_run_event`'s `cost_usd` line or `enqueue_task`; M5 owns those. Coordinate by editing
different functions in `relay_result.rs`: you own the `input`/`output`/metadata fields, M5 owns
`cost_usd`/`cost_source`).

## Gates
`cargo build/test/clippy` for lighttrack-agent, -api, -runner, -store, -store-pg; `tests_relay.rs`
extended; SQLite conformance if the store changes.

## Evaluation
Before: relay events `input: None, output: None`; judges skip 100% of relay traffic; action
prompts unversioned. After: opted-in relay events carry content and get scored (`lt-runner score`
reports `scored > 0` for tag `relay` in a test); `GET /v1/relay/actions` lists fingerprints.
