"""LightTrack Python client — fire-and-forget LLM event ingestion.

See `lighttrack.client` for the API. Quick start:

    from lighttrack import LightTrack
    lt = LightTrack()                      # reads LIGHTTRACK_URL + LIGHTTRACK_KEY from env

    resp = openai_client.chat.completions.create(...)
    lt.track_openai(resp, latency_ms=120)  # non-blocking, best-effort

    lt.close()                             # flush on shutdown (also auto-runs at exit)

Or auto-instrument the provider SDKs so every call is captured with no per-call code:

    import lighttrack.auto                 # patch OpenAI / Anthropic / Gemini globally
    # ...or wrap one client instance:
    from lighttrack import wrap
    client = wrap(openai_client)

    from lighttrack import trace
    with trace():                          # calls inside share one trace_id
        client.chat.completions.create(...)
"""

from .client import GuardResult, LightTrack, RelayError, Span, guard
from .instrument import (
    current_span_id,
    current_trace_id,
    instrument,
    span,
    trace,
    uninstrument,
    wrap,
)

__all__ = [
    "LightTrack",
    "Span",
    "guard",
    "GuardResult",
    "RelayError",
    "instrument",
    "wrap",
    "uninstrument",
    "trace",
    "span",
    "current_trace_id",
    "current_span_id",
]
__version__ = "0.1.0"
