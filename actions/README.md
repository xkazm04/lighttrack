# Action library (lt-agent)

Each subdirectory is one **action type** the relay can invoke on this device — the executable
half of the contract described in `docs/RELAY.md`. The cloud only ever sends
`action_type` + JSON params; the prompt, model choice, and result connector live here and are
**gitignored** (except this README and `_example/`), so every user builds their own library and
credentials never leave the machine.

## Layout

```
actions/
  <namespace>/<action-name>/     # action_type "xprice/reprice-summary" → actions/xprice/reprice-summary/
    prompt.md                    # required — template; {{params.<key>}}, {{payload}}, {{task_id}}, {{action_type}}
    action.toml                  # optional — model + options + connector (all-defaults if absent)
    schema.json                  # optional — JSON schema; result becomes conforming JSON, not text
```

## `action.toml`

```toml
model = "sonnet@high"        # optional @effort suffix: low|medium|high|xhigh|max (default "sonnet")
system = "You are …"         # optional system prompt
schema_file = "schema.json"  # optional structured output
version = "3"                # optional label you bump when you edit prompt.md
report_io = false            # optional — send the prompt and result to the cloud so runs are judged

# — posture: what this action's run is allowed to be (default "generate") —
mode = "generate"            # generate | readonly-scan | edit
workspace = "my-repo"        # required by readonly-scan/edit; resolved under `workspaces_root`
allowed_tools = []           # extras on top of the mode's base allowlist
permission_mode = "default"  # scan: plan|default (optional). edit: REQUIRED (e.g. "acceptEdits")
max_budget_usd = 0.50        # optional per-run spend ceiling
timeout_secs = 300           # optional wall clock (default 600)

[connector]                  # optional — how the result reaches the originating app
kind = "http"                # POST the result envelope as JSON
url = "https://my-app.example/internal/relay-results"
headers = { authorization = "Bearer ${MY_APP_CALLBACK_KEY}" }   # ${ENV} expanded at delivery

# — or —
# kind = "command"           # pipe the envelope to a local script's stdin (any DB / bespoke API)
# command = ["python", "push_result.py"]
```

The delivered envelope is
`{ task_id, action_type, idempotency_key, source, params, result, model }`. Relay delivery is
**at-least-once** — a retried task re-runs the action — so connectors must be idempotent; key
writes on `idempotency_key` (or `task_id`).

## `version` and `report_io` — making an action's prompt reviewable

`prompt.md` is edited in place, on this machine, with no history the cloud can see. Two optional
keys close that, and the privacy default is **off**.

Every settle report carries `prompt_sha256`: sha256 of the **rendered** prompt — params
substituted, so it is the text the model actually read. That is not optional and costs nothing to
send: it is 64 hex characters, no content, and it is what lets `GET /v1/relay/actions` show that
this action ran one prompt until the 14th and a different one since. `version` is the label you
bump beside it (`"3"`, `"2026-09-02"`, `"v2-tighter-rubric"`); the cloud never parses it, it only
groups by it, so it is there to make the change legible to a human reading the ledger.

`report_io = true` additionally sends the rendered prompt and the result text, which the cloud
stores as the run's event `input`/`output`. That is what makes the run **judgeable**: both of
LightTrack's judges skip an event with no content, so an action that has not opted in is never
scored. Turn it on per action, deliberately — it moves that action's prompt and result into your
cloud instance, where the usual ingest PII scrub applies to them like any other captured payload.
With it off the cloud holds the fingerprint and nothing else.

Independently of `report_io`, an action's succeeded runs can be snapshotted into an eval dataset
(`POST /v1/relay/actions/<action_type>/dataset`, admin) and a benchmark linked to it, so the next
edit to `prompt.md` is gated the way a registry prompt's promotion is. That reads the task's
`payload` and `result`, which the cloud has anyway — the prompt text is never needed for it. See
`docs/RELAY.md`, "Quality model".

## Posture (`mode`)

Every action declares what its run may touch, and the agent enforces it through the engine's one
invocation seam **before** the CLI is spawned — a mistake here costs nothing, rather than being
discovered in a diff. See the matrix in `docs/RELAY.md`.

| `mode` | workspace | tools | `permission_mode` |
|---|---|---|---|
| `generate` (default) | forbidden — runs in a neutral temp dir with no ambient `CLAUDE.md` | none | forbidden |
| `readonly-scan` | **required** | `Read`/`Glob`/`Grep`/`LS` + your extras, each of which must be read-only | optional, `plan` or `default` |
| `edit` | **required** | your list (no base set) | **required** |

`workspace` is a name relative to `workspaces_root` in `agent.toml`, validated exactly like
`action_type` — no absolute paths, no `..`, no backslashes — and it must already exist. If
`workspaces_root` is unset this device runs no scan or edit actions at all; opting in is a
directory the operator names, so the reachable set is theirs, not the cloud's. The cloud still
only ever sends `action_type` + params.

A `readonly-scan` that lists a write-capable tool (`Write`, `Edit`, `Bash(git push:*)`, …) is a
posture error, not a warning: anything the allowlist doesn't recognise counts as write-capable.

Start by copying `_example/echo/` (a plain completion) or `_example/readonly-scan/` (a
repository-reading run) and editing from there.
