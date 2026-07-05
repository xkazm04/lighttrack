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

Start by copying `_example/echo/` and editing from there.
