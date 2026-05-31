"""LightTrack client: wrap OpenAI / Anthropic / Gemini results and POST a normalized event.

Design goals:
- **Never break your app.** Every send is best-effort; exceptions are swallowed.
- **Never block the request path.** Events go on a background daemon thread by default; if the queue
  is full they are dropped rather than blocking the caller.
- **Zero third-party dependencies** (stdlib only) so it drops into any project.

The API derives the project from the API key and fills id / timestamp / cost, so the minimal event is
just `{provider, model, usage}`.
"""

from __future__ import annotations

import atexit
import json
import os
import queue
import threading
import time
import urllib.request
from typing import Any, Optional

_DEFAULT_URL = "http://127.0.0.1:8787"

# Map common provider names/aliases onto the API's enum (openai|anthropic|google; else "unknown").
_PROVIDER_ALIASES = {
    "openai": "openai", "azure": "openai", "azure_openai": "openai", "oai": "openai",
    "anthropic": "anthropic", "claude": "anthropic",
    "google": "google", "gemini": "google", "vertex": "google", "vertexai": "google",
    "google-genai": "google", "genai": "google",
}


def _norm_provider(p: Any) -> str:
    s = str(p).strip().lower()
    return _PROVIDER_ALIASES.get(s, s)


def _get(obj: Any, *names: str) -> Any:
    """First present attribute or dict key from `obj` (handles SDK objects and plain dicts)."""
    if obj is None:
        return None
    for n in names:
        if isinstance(obj, dict):
            if n in obj:
                return obj[n]
        elif hasattr(obj, n):
            return getattr(obj, n)
    return None


def _extract_openai(resp: Any):
    usage = _get(resp, "usage")
    inp = _get(usage, "prompt_tokens", "input_tokens") or 0
    out = _get(usage, "completion_tokens", "output_tokens") or 0
    cached = _get(_get(usage, "prompt_tokens_details"), "cached_tokens")
    return (_get(resp, "model"), int(inp), int(out), cached)


def _extract_anthropic(resp: Any):
    usage = _get(resp, "usage")
    inp = _get(usage, "input_tokens") or 0
    out = _get(usage, "output_tokens") or 0
    cached = _get(usage, "cache_read_input_tokens")
    return (_get(resp, "model"), int(inp), int(out), cached)


def _extract_gemini(resp: Any):
    um = _get(resp, "usage_metadata", "usageMetadata")
    inp = _get(um, "prompt_token_count", "promptTokenCount") or 0
    out = _get(um, "candidates_token_count", "candidatesTokenCount") or 0
    cached = _get(um, "cached_content_token_count", "cachedContentTokenCount")
    return (_get(resp, "model_version", "modelVersion"), int(inp), int(out), cached)


class LightTrack:
    def __init__(self, base_url: Optional[str] = None, api_key: Optional[str] = None, *,
                 project: Optional[str] = None, source: Optional[str] = None,
                 tags: Optional[list] = None, enabled: bool = True, async_: bool = True,
                 timeout: float = 2.0, max_queue: int = 1000):
        self.base_url = (base_url or os.environ.get("LIGHTTRACK_URL", _DEFAULT_URL)).rstrip("/")
        self.api_key = api_key or os.environ.get("LIGHTTRACK_KEY") or None
        # A project key derives the project server-side; set `project` only for dev mode (no key) or
        # an admin key ingesting into a specific project.
        self.project = project or os.environ.get("LIGHTTRACK_PROJECT") or None
        self.source = source
        self.default_tags = list(tags or [])
        self.enabled = enabled
        self.timeout = timeout
        self._async = async_
        self._q: "queue.Queue[Optional[dict]]" = queue.Queue(maxsize=max_queue)
        self._closed = False
        self._worker: Optional[threading.Thread] = None
        if enabled and async_:
            self._worker = threading.Thread(target=self._run, name="lighttrack", daemon=True)
            self._worker.start()
            atexit.register(self.close)

    # ---- public API ----
    def track(self, provider: str, model: Optional[str], *, input_tokens: int = 0,
              output_tokens: int = 0, cached_input: Optional[int] = None,
              operation: Optional[str] = None, latency_ms: Optional[int] = None,
              status: Optional[str] = None, error: Optional[str] = None, input: Any = None,
              output: Any = None, tags: Optional[list] = None, trace_id: Optional[str] = None,
              metadata: Any = None, project: Optional[str] = None) -> None:
        """Record one LLM call. Returns immediately; the event is sent best-effort."""
        if not self.enabled:
            return
        usage = {"input": int(input_tokens or 0), "output": int(output_tokens or 0)}
        if cached_input is not None:
            usage["cached_input"] = int(cached_input)
        ev: dict = {"provider": _norm_provider(provider), "model": model or "unknown", "usage": usage}
        pid = project or self.project
        if pid:
            ev["project_id"] = pid
        if operation:
            ev["operation"] = operation
        if latency_ms is not None:
            ev["latency_ms"] = int(latency_ms)
        if error:
            ev["error"] = error
            status = status or "error"
        if status:
            ev["status"] = status
        if input is not None:
            ev["input"] = input
        if output is not None:
            ev["output"] = output
        all_tags = self.default_tags + list(tags or [])
        if all_tags:
            ev["tags"] = all_tags
        if trace_id:
            ev["trace_id"] = trace_id
        if self.source:
            ev["source"] = self.source
        if metadata:
            ev["metadata"] = metadata
        self._emit(ev)

    def track_openai(self, response: Any, *, model: Optional[str] = None, **kw) -> None:
        m, i, o, c = _extract_openai(response)
        self.track("openai", model or m, input_tokens=i, output_tokens=o, cached_input=c, **kw)

    def track_anthropic(self, response: Any, *, model: Optional[str] = None, **kw) -> None:
        m, i, o, c = _extract_anthropic(response)
        self.track("anthropic", model or m, input_tokens=i, output_tokens=o, cached_input=c, **kw)

    def track_gemini(self, response: Any, *, model: Optional[str] = None, **kw) -> None:
        m, i, o, c = _extract_gemini(response)
        self.track("google", model or m, input_tokens=i, output_tokens=o, cached_input=c, **kw)

    def span(self, provider: str, model: Optional[str], **kw) -> "Span":
        """Time a call and auto-track on exit: `with lt.span("openai","gpt-4o") as s: ...; s.set_openai(resp)`."""
        return Span(self, provider, model, **kw)

    def flush(self, timeout: float = 5.0) -> None:
        if not (self.enabled and self._async):
            return
        deadline = time.monotonic() + timeout
        while not self._q.empty() and time.monotonic() < deadline:
            time.sleep(0.01)

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        if self._worker:
            self.flush()
            self._q.put(None)  # sentinel: stop the worker
            self._worker.join(timeout=self.timeout + 1.0)

    def __enter__(self) -> "LightTrack":
        return self

    def __exit__(self, *exc) -> bool:
        self.close()
        return False

    # ---- internals ----
    def _emit(self, ev: dict) -> None:
        if self._async:
            try:
                self._q.put_nowait(ev)
            except queue.Full:
                pass  # drop rather than block the caller
        else:
            self._post(ev)

    def _run(self) -> None:
        while True:
            ev = self._q.get()
            if ev is None:
                self._q.task_done()
                break
            self._post(ev)
            self._q.task_done()

    def _post(self, ev: dict) -> None:
        try:
            data = json.dumps(ev).encode("utf-8")
            headers = {"Content-Type": "application/json"}
            if self.api_key:
                headers["Authorization"] = f"Bearer {self.api_key}"
            req = urllib.request.Request(f"{self.base_url}/v1/events", data=data, headers=headers, method="POST")
            with urllib.request.urlopen(req, timeout=self.timeout):
                pass
        except Exception:
            pass  # best-effort: telemetry must never break the host app


class Span:
    """A timing context manager that tracks one call on exit (latency measured automatically)."""

    def __init__(self, client: LightTrack, provider: str, model: Optional[str], **kw):
        self._c = client
        self._provider = provider
        self._model = model
        self._kw = kw
        self._usage = {"input_tokens": 0, "output_tokens": 0, "cached_input": None}
        self._t0: Optional[float] = None

    def __enter__(self) -> "Span":
        self._t0 = time.perf_counter()
        return self

    def set_usage(self, input_tokens: int = 0, output_tokens: int = 0, cached_input: Optional[int] = None) -> "Span":
        self._usage = {"input_tokens": input_tokens, "output_tokens": output_tokens, "cached_input": cached_input}
        return self

    def set_openai(self, resp: Any) -> "Span":
        m, i, o, c = _extract_openai(resp)
        self._model = self._model or m
        return self.set_usage(i, o, c)

    def set_anthropic(self, resp: Any) -> "Span":
        m, i, o, c = _extract_anthropic(resp)
        self._model = self._model or m
        return self.set_usage(i, o, c)

    def set_gemini(self, resp: Any) -> "Span":
        m, i, o, c = _extract_gemini(resp)
        self._model = self._model or m
        return self.set_usage(i, o, c)

    def __exit__(self, exc_type, exc, tb) -> bool:
        latency = int((time.perf_counter() - self._t0) * 1000) if self._t0 is not None else None
        self._c.track(
            self._provider, self._model, latency_ms=latency,
            status="error" if exc_type else None,
            error=str(exc) if exc else None,
            input_tokens=self._usage["input_tokens"], output_tokens=self._usage["output_tokens"],
            cached_input=self._usage["cached_input"], **self._kw,
        )
        return False  # never suppress the caller's exception
