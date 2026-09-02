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
