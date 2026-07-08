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
import re
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
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


# ---- Output guardrails ------------------------------------------------------

_PII_PATTERNS = [
    ("email", re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")),
    ("phone", re.compile(r"(?:\+?\d[\s().-]?){10,}")),
    ("credit_card", re.compile(r"\b(?:\d[ -]?){13,16}\b")),
    ("ssn", re.compile(r"\b\d{3}-\d{2}-\d{4}\b")),
]


@dataclass
class GuardResult:
    ok: bool
    violations: list
    checks: dict = field(default_factory=dict)


def guard(output: str, rules: dict) -> GuardResult:
    """Deterministic, network-free output validation — runs inline in the request path.

    Pure: returns a verdict; the caller decides what to do (retry / fallback / block). Mirrors the
    TS/Rust `guard`. Supported `rules` keys: `json` (bool), `json_keys` (list[str], implies json),
    `max_words`, `min_words`, `max_chars`, `must_include` (list[str]), `must_match` (regex str),
    `must_not_match` (list[regex str]), `no_pii` (bool).
    """
    violations: list = []
    checks: dict = {}

    def record(key: str, passed: bool, msg: str = "") -> None:
        checks[key] = passed
        if not passed:
            violations.append(msg)

    json_keys = rules.get("json_keys") or []
    want_json = bool(rules.get("json")) or len(json_keys) > 0
    parsed = None
    if want_json:
        try:
            parsed = json.loads(output.strip())
            record("json", True)
        except Exception:
            record("json", False, "output is not valid JSON")
    if json_keys and isinstance(parsed, dict):
        for k in json_keys:
            record(f"key:{k}", k in parsed, f"missing required JSON key '{k}'")

    stripped = output.strip()
    words = len(stripped.split()) if stripped else 0
    if (mw := rules.get("max_words")) is not None:
        record("max_words", words <= mw, f"too long: {words} words > {mw}")
    if (mnw := rules.get("min_words")) is not None:
        record("min_words", words >= mnw, f"too short: {words} words < {mnw}")
    if (mc := rules.get("max_chars")) is not None:
        record("max_chars", len(output) <= mc, f"too long: {len(output)} chars > {mc}")
    for s in rules.get("must_include") or []:
        record(f"include:{s}", s in output, f'must include "{s}"')
    if (mm := rules.get("must_match")) is not None:
        record("must_match", re.search(mm, output) is not None, f"must match {mm}")
    for pat in rules.get("must_not_match") or []:
        record(f"not_match:{pat}", re.search(pat, output) is None, f"must not match {pat}")
    if rules.get("no_pii"):
        clean = True
        for name, rx in _PII_PATTERNS:
            if rx.search(output):
                clean = False
                record(f"pii:{name}", False, f"contains {name}-like PII")
        if clean:
            record("no_pii", True)

    return GuardResult(ok=len(violations) == 0, violations=violations, checks=checks)


class RelayError(Exception):
    """A relay call failed (network error or non-2xx). Unlike telemetry, relay enqueue/status is a
    functional call the app depends on, so failures raise instead of being swallowed."""


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
        self._q: "queue.Queue[Optional[tuple]]" = queue.Queue(maxsize=max_queue)
        self._closed = False
        self._worker: Optional[threading.Thread] = None
        if enabled and async_:
            self._worker = threading.Thread(target=self._run, name="lighttrack", daemon=True)
            self._worker.start()
            atexit.register(self.close)

    # ---- public API ----
    def track(self, provider: str, model: Optional[str], *, name: Optional[str] = None, input_tokens: int = 0,
              output_tokens: int = 0, cached_input: Optional[int] = None,
              operation: Optional[str] = None, latency_ms: Optional[int] = None,
              status: Optional[str] = None, error: Optional[str] = None, input: Any = None,
              output: Any = None, tags: Optional[list] = None, trace_id: Optional[str] = None,
              span_id: Optional[str] = None, parent_span_id: Optional[str] = None,
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
        if name:
            ev["name"] = name
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
        if span_id:
            ev["span_id"] = span_id
        if parent_span_id:
            ev["parent_span_id"] = parent_span_id
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

    def track_guard(self, output: str, rules: dict, *, name: Optional[str] = None,
                    project: Optional[str] = None) -> GuardResult:
        """Validate `output` against `guard` rules and record the verdict as a score (fire-and-forget)
        so guardrail pass-rates are observable. Returns the verdict so the caller can act
        (retry / fallback / block). Never blocks or raises."""
        result = guard(output, rules)
        if self.enabled:
            score: dict = {
                "rubric": f"guard:{name}" if name else "guard",
                "value": 1 if result.ok else 0,
                "max": 1,
                "pass": result.ok,
                "reasoning": "; ".join(result.violations) or "all checks passed",
                "scored_by": f"guard:{self.source}" if self.source else "lighttrack-guard",
            }
            pid = project or self.project
            if pid:
                score["project_id"] = pid
            self._emit(score, "/v1/scores")
        return result

    # ---- relay (cloud→device tasks; docs/RELAY.md) ----
    def relay_task(self, action_type: str, payload: Any = None, *,
                   idempotency_key: Optional[str] = None, source: Optional[str] = None,
                   max_attempts: Optional[int] = None, retry_interval_secs: Optional[int] = None,
                   project: Optional[str] = None) -> dict:
        """Enqueue a task for the enrolled local device (executed via Claude Code, offline-tolerant).
        Synchronous; returns the task dict (re-enqueueing an `idempotency_key` returns the existing
        task). Raises `RelayError` on failure."""
        body: dict = {"action_type": action_type}
        if payload is not None:
            body["payload"] = payload
        if idempotency_key:
            body["idempotency_key"] = idempotency_key
        if source or self.source:
            body["source"] = source or self.source
        if max_attempts is not None:
            body["max_attempts"] = int(max_attempts)
        if retry_interval_secs is not None:
            body["retry_interval_secs"] = int(retry_interval_secs)
        pid = project or self.project
        if pid:
            body["project_id"] = pid
        return self._request("POST", "/v1/relay/tasks", body)

    def get_relay_task(self, task_id: str) -> dict:
        """Fetch one relay task (status / result / error). Raises `RelayError` on failure."""
        return self._request("GET", f"/v1/relay/tasks/{task_id}")

    def wait_relay_task(self, task_id: str, *, timeout: float = 1200.0,
                        interval: float = 15.0) -> dict:
        """Poll until the task settles (`succeeded` | `dead`) or `timeout` elapses; returns the last
        seen task either way. Relay work is offline-tolerant by design — retries may span 5h
        windows — so only wait on tasks you expect the device to pick up promptly."""
        deadline = time.monotonic() + timeout
        while True:
            task = self.get_relay_task(task_id)
            if task.get("status") in ("succeeded", "dead") or time.monotonic() >= deadline:
                return task
            time.sleep(min(interval, max(0.0, deadline - time.monotonic())))

    def span(self, provider: str, model: Optional[str], **kw) -> "Span":
        """Time a call and auto-track on exit: `with lt.span("openai","gpt-4o") as s: ...; s.set_openai(resp)`."""
        return Span(self, provider, model, **kw)

    def wrap(self, client: Any) -> Any:
        """Auto-instrument an OpenAI / Anthropic / Gemini SDK client *instance* so every call it makes
        is tracked through this client. Returns the same object (drop-in): `client = lt.wrap(OpenAI())`."""
        from .instrument import wrap as _wrap
        return _wrap(client, lt=self)

    def instrument(self, providers: Optional[list] = None) -> "LightTrack":
        """Monkey-patch the installed provider SDK *classes* so every client auto-tracks through this
        instance. `providers` optionally restricts to a subset (e.g. `["openai"]`)."""
        from .instrument import instrument as _instrument
        return _instrument(self, providers=providers)

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
    def _emit(self, body: dict, path: str = "/v1/events") -> None:
        if self._async:
            try:
                self._q.put_nowait((path, body))
            except queue.Full:
                pass  # drop rather than block the caller
        else:
            self._post(path, body)

    def _run(self) -> None:
        while True:
            item = self._q.get()
            if item is None:
                self._q.task_done()
                break
            path, body = item
            self._post(path, body)
            self._q.task_done()

    def _request(self, method: str, path: str, body: Optional[dict] = None) -> dict:
        """Synchronous request that returns the parsed JSON response and raises `RelayError` on
        any failure — for functional calls (relay), not telemetry."""
        headers = {"Content-Type": "application/json"}
        if self.api_key:
            headers["Authorization"] = f"Bearer {self.api_key}"
        data = json.dumps(body).encode("utf-8") if body is not None else None
        req = urllib.request.Request(f"{self.base_url}{path}", data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=max(self.timeout, 10.0)) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as e:
            raise RelayError(f"{method} {path} -> HTTP {e.code}: {e.read().decode('utf-8', 'replace')}") from e
        except Exception as e:
            raise RelayError(f"{method} {path} failed: {e}") from e

    def _post(self, path: str, body: dict) -> None:
        try:
            data = json.dumps(body).encode("utf-8")
            headers = {"Content-Type": "application/json"}
            if self.api_key:
                headers["Authorization"] = f"Bearer {self.api_key}"
            req = urllib.request.Request(f"{self.base_url}{path}", data=data, headers=headers, method="POST")
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
