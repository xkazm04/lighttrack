# lighttrack-client (Python)

Fire-and-forget client for ingesting LLM call events into [LightTrack](https://github.com/xkazm04/lighttrack).
Stdlib only (no third-party dependencies); sends happen on a background thread and never raise into
your app.

**What this SDK can do, next to the other two:** the generated capability matrix in
[`clients/README.md`](../README.md#what-each-sdk-can-do) — rendered from
[`lighttrack.manifest.json`](lighttrack.manifest.json), not from prose. The behaviour it shares with
the TypeScript and Rust clients is pinned by [`clients/contract/`](../contract/) and asserted here by
`tests/test_contract.py`.

## Install

```bash
pip install ./clients/python      # or: pip install lighttrack-client (once published)
```

## Configure

**Where do my events land?** Every event is attributed to a project. A **project key** pins it
server-side; otherwise the event has to name one. With neither, a **dev-mode** server files events
under a `default` project — fine for a first run — while a server with **authentication enabled**
rejects them. Set one of these as soon as you want events somewhere specific:

```bash
export LIGHTTRACK_URL=http://127.0.0.1:8787   # default; override for a remote server
export LIGHTTRACK_PROJECT=demo                # choose the project (also needed with an admin key)
# ...or instead: export LIGHTTRACK_KEY=lt_...  # a project key pins the project server-side
```

## Use

```python
from lighttrack import LightTrack

lt = LightTrack(source="my-app")           # reads LIGHTTRACK_URL / LIGHTTRACK_KEY / LIGHTTRACK_PROJECT
# ...or pass it explicitly, no env needed:
# lt = LightTrack(project="demo", source="my-app")

resp = openai_client.chat.completions.create(model="gpt-4o", messages=[...])
lt.track_openai(resp, latency_ms=120)      # also: track_anthropic, track_gemini, generic track(...)

lt.close()                                  # flush at shutdown (auto-runs at exit too)
```

`with LightTrack() as lt:` flushes on exit. `lt.span(provider, model)` times a call and tracks it
automatically. See `example.py` for a runnable demo and the repo's `clients/README.md` for details.

### Calls that die mid-flight

An event carries usage and an outcome, so it is sent when the call *finishes*. That alone would mean
a process killed mid-call (OOM killer, SIGKILL, a container eviction) leaves no record of a call that
definitely happened and definitely cost money — the exact calls you most want afterwards.

So a span also writes a small **crash-surviving breadcrumb** when it opens, and retires it on every
exit path. The next LightTrack client to start with the same journal directory re-reports anything
left unsettled, as `status="error"` with the reason spelled out and a `lighttrack:unsettled-span`
tag — never as a clean zero-cost success, because nothing ever reported an outcome.

Knobs, and the honest limit: `LIGHTTRACK_JOURNAL=0` (or `LightTrack(journal=False)`) turns it off;
`LIGHTTRACK_JOURNAL_DIR` chooses where breadcrumbs live; `LIGHTTRACK_JOURNAL_ORPHAN_SECS` (default
300) is how long another process's journal must sit untouched before it is treated as abandoned.
Recovery needs a later client to see that directory — a container that dies and is rescheduled onto
fresh storage is not covered unless you point the directory at a mounted volume.

## Why don't I see my events?

`track*` never raises — but it is not silent. A failed send writes one actionable line to **stderr**
(never stdout, which your app may be using as a protocol channel):

```
[lighttrack] no project is configured, so these events are not attributed: a dev-mode server files
them under the 'default' project, and a server with authentication enabled rejects them. To choose
where they land, set LIGHTTRACK_PROJECT=<your-project-id> ...
```

That case is reported *before* the request, so it appears on your very first `track*`. Warnings are
rate-limited to one line per error kind per 60 s, so a hot loop costs one line, not thousands.

Silence them with `LIGHTTRACK_QUIET=1` or `LightTrack(quiet=True)`.

## Test

```bash
cd clients/python && python -m unittest discover tests
```

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
