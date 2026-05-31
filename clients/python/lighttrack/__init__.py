"""LightTrack Python client — fire-and-forget LLM event ingestion.

See `lighttrack.client` for the API. Quick start:

    from lighttrack import LightTrack
    lt = LightTrack()                      # reads LIGHTTRACK_URL + LIGHTTRACK_KEY from env

    resp = openai_client.chat.completions.create(...)
    lt.track_openai(resp, latency_ms=120)  # non-blocking, best-effort

    lt.close()                             # flush on shutdown (also auto-runs at exit)
"""

from .client import LightTrack, Span

__all__ = ["LightTrack", "Span"]
__version__ = "0.1.0"
