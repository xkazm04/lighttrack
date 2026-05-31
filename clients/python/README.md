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
