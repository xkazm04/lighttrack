# lighttrack-client (Python)

Fire-and-forget client for ingesting LLM call events into [LightTrack](https://github.com/xkazm04/tracklight).
Stdlib only (no third-party dependencies); sends happen on a background thread and never raise into
your app.

## Install

```bash
pip install ./clients/python      # or: pip install lighttrack-client (once published)
```

## Use

```python
from lighttrack import LightTrack

lt = LightTrack(source="my-app")           # reads LIGHTTRACK_URL / LIGHTTRACK_KEY / LIGHTTRACK_PROJECT

resp = openai_client.chat.completions.create(model="gpt-4o", messages=[...])
lt.track_openai(resp, latency_ms=120)      # also: track_anthropic, track_gemini, generic track(...)

lt.close()                                  # flush at shutdown (auto-runs at exit too)
```

`with LightTrack() as lt:` flushes on exit. `lt.span(provider, model)` times a call and tracks it
automatically. See `example.py` for a runnable demo and the repo's `clients/README.md` for details.

## Auto-instrument (one line)

Skip the per-call `track*`. Patch the installed provider SDKs globally with a single import, or wrap
one client instance — every call is then captured automatically (model, usage, latency, trace ids):

```python
import lighttrack.auto                  # patch OpenAI / Anthropic / Gemini SDK clients globally
resp = openai_client.chat.completions.create(...)   # auto-tracked

# ...or instrument a single client instance and group calls into a trace:
from lighttrack import wrap, trace, span
client = wrap(openai_client)
with trace():                           # calls inside share one trace_id
    client.chat.completions.create(...)
    with span():                        # calls inside link to it via parent_span_id
        client.chat.completions.create(...)
```

Trace context propagates via `contextvars`. Best-effort: instrumentation never breaks your call.

## Relay tasks (offline device work)

Enqueue heavy, offline-tolerant LLM tasks for the enrolled local device running `lt-agent`
(executed via Claude Code on subscription; see `docs/RELAY.md`). Unlike `track*` telemetry these
are functional calls: they return the task and raise `RelayError` on failure.

```python
task = lt.relay_task("xprice/reprice-summary", {"sku": "A-1"}, idempotency_key="order-42")
task = lt.wait_relay_task(task["id"])       # optional poll; prefer the action's connector push
if task["status"] == "succeeded":
    print(task["result"])
```
