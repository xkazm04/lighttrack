"""LightTrack client: wrap OpenAI / Anthropic / Gemini results and POST a normalized event.

Design goals:
- **Never break your app.** Every send is best-effort; exceptions are swallowed — but a failure is
  reported on stderr (rate-limited, `LIGHTTRACK_QUIET=1` to silence) instead of vanishing.
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
import urllib.parse
import urllib.request
import uuid
from dataclasses import dataclass, field
from typing import Any, Optional

from .admission import (Admit, AdmissionCache, BudgetExceeded, DEFAULT_ADMISSION_TTL_MS,
                        view_from_statuses)
from .diagnostics import (Diagnostics, diagnostic_kind, no_project_message, send_failure_message,
                          truncate)
from .journal import RECOVERED_TAG, SpanJournal, unsettled_error
from .limits import parse_limit_view
from .pii import PII_RULES

_DEFAULT_URL = "http://127.0.0.1:8787"

#: Tag on the zero-usage event a locally-blocked call leaves behind.
BLOCKED_TAG = "lt_blocked_locally"

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
    # The Responses API renamed the pair AND moved the cache counter: `input_tokens_details`, not
    # `prompt_tokens_details`. Reading only the older place reported every cached Responses call as
    # uncached, which the price book then charged at full input rate.
    cached = _get(_get(usage, "prompt_tokens_details"), "cached_tokens")
    if cached is None:
        cached = _get(_get(usage, "input_tokens_details"), "cached_tokens")
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

#: Compiled once. `PII_RULES` is generated from the server's own scrubber (see `lighttrack.pii`).
_PII_COMPILED = [(r["kind"], re.compile(r["pattern"])) for r in PII_RULES]


def pii_kinds(text: str) -> list:
    """The PII families present in `text`, in rule order, each reported once.

    A kind is a family, not a regex: a phone number has three shapes and a secret three prefixes, and
    a caller wants to know *a phone number leaked*, not which of three patterns noticed.
    """
    found = []
    for kind, rx in _PII_COMPILED:
        if kind not in found and rx.search(text):
            found.append(kind)
    return found


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
        kinds = pii_kinds(output)
        for kind in kinds:
            record(f"pii:{kind}", False, f"contains {kind}-like PII")
        if not kinds:
            record("no_pii", True)

    return GuardResult(ok=len(violations) == 0, violations=violations, checks=checks)


def _error_body(e: "urllib.error.HTTPError") -> str:
    """The server's explanation of a rejection — that string is the whole point of the diagnostic."""
    try:
        return truncate(e.read().decode("utf-8", "replace"))
    except Exception:
        return ""


class RelayError(Exception):
    """A relay call failed (network error or non-2xx). Unlike telemetry, relay enqueue/status is a
    functional call the app depends on, so failures raise instead of being swallowed.

    `code` carries the API's own error code when the response had one, so a caller can act on the
    *kind* of failure instead of pattern-matching a message. The one worth branching on is
    `relay_unroutable` (`is_unroutable`): no enrolled device advertises that action type, so unlike
    a timeout or a 503 the call will not succeed on a retry -- the fix is the action type's
    spelling, or a device's advertised capabilities."""

    def __init__(self, message: str, *, code: Optional[str] = None,
                 status: Optional[int] = None):
        super().__init__(message)
        self.code = code
        self.status = status

    @property
    def is_unroutable(self) -> bool:
        """Whether nothing in the fleet can ever run that action type (HTTP 422)."""
        return self.code == "relay_unroutable"


def error_code(body: str) -> Optional[str]:
    """The API's error code out of an error body, or None when the body is not one.

    Deliberately total: an error response is exactly when a body is least likely to be well-formed
    (a proxy's HTML, a truncated stream), and a parse failure here must degrade to "no code" rather
    than replace the real failure with a JSON error nobody can act on."""
    try:
        code = json.loads(body).get("error", {}).get("code")
    except Exception:
        return None
    return code if isinstance(code, str) else None


class LightTrack:
    def __init__(self, base_url: Optional[str] = None, api_key: Optional[str] = None, *,
                 project: Optional[str] = None, source: Optional[str] = None,
                 tags: Optional[list] = None, enabled: bool = True, async_: bool = True,
                 timeout: float = 2.0, max_queue: int = 1000, quiet: Optional[bool] = None,
                 journal: Optional[bool] = None, journal_dir: Optional[str] = None,
                 enforce: Optional[str] = None, admission_ttl_ms: int = DEFAULT_ADMISSION_TTL_MS,
                 record_blocked: bool = False):
        """`quiet=True` (or `LIGHTTRACK_QUIET=1`) suppresses the stderr diagnostics that a dropped or
        rejected event otherwise reports; `None` defers to the env var.

        `journal=False` (or `LIGHTTRACK_JOURNAL=0`) turns off the crash-surviving breadcrumb that
        makes a call which began but never reported an outcome recoverable — see `journal.py` for
        what that costs and what it buys.

        `enforce` turns on pre-spend admission (see `admission.py`): `"block"` raises
        `BudgetExceeded` instead of making a call the project's caps would turn away, `"warn"` logs
        and proceeds, `"off"` (the default, also read from `LIGHTTRACK_ENFORCE`) only observes. Off
        by default deliberately: adding an observability SDK must not change what an app does.

        `record_blocked=True` records a locally-blocked call as a zero-usage event tagged
        `lt_blocked_locally` — it is not spend and is never recorded as spend, but it is traffic the
        app attempted, and a rollup that cannot see it reads as a quiet week rather than a throttled
        one."""
        self.base_url = (base_url or os.environ.get("LIGHTTRACK_URL", _DEFAULT_URL)).rstrip("/")
        self.api_key = api_key or os.environ.get("LIGHTTRACK_KEY") or None
        # A project key derives the project server-side; set `project` only for dev mode (no key) or
        # an admin key ingesting into a specific project.
        self.project = project or os.environ.get("LIGHTTRACK_PROJECT") or None
        self.source = source
        self.default_tags = list(tags or [])
        self.enabled = enabled
        self.timeout = timeout
        self.diag = Diagnostics(quiet)
        self._async = async_
        self._q: "queue.Queue[Optional[tuple]]" = queue.Queue(maxsize=max_queue)
        self._closed = False
        self._worker: Optional[threading.Thread] = None
        self.journal = SpanJournal(enabled=journal, directory=journal_dir)
        self.enforce = enforce or os.environ.get("LIGHTTRACK_ENFORCE") or "off"
        self.record_blocked = record_blocked
        #: What the server last said about this project's caps, and the pre-spend verdict taken from
        #: it. Public so a host app can ask `lt.limits.admit(...)` directly, or seed it in a test.
        self.limits = AdmissionCache(ttl_ms=admission_ttl_ms)
        self._refresh_lock = threading.Lock()
        if enabled and async_:
            self._worker = threading.Thread(target=self._run, name="lighttrack", daemon=True)
            self._worker.start()
            atexit.register(self.close)
        if enabled:
            # Report the calls a previous process began and never settled, before anything else this
            # client sends. This is the whole point of the journal: it is a *later* client on the
            # same machine that turns a killed process's in-flight calls back into records.
            self.recover_unsettled_spans()

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
        task). Raises `RelayError` on failure.

        The returned task carries an `admission` verdict (M18): `{"verdict": "queued",
        "eligible_devices": N}` says how much of the fleet advertises this action type -- `0` means
        no devices are enrolled at all (the legacy single-device deployment), never that an enrolled
        fleet declined it. An action type nothing advertises does not come back as a task at all: it
        raises `RelayError` with `is_unroutable`, because a queued task nothing can lease is a
        slow-motion dead letter."""
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

    def recover_unsettled_spans(self) -> int:
        """Turn every abandoned journal breadcrumb into a real event and return how many were sent.

        Called automatically at construction; exposed so a supervisor that never constructs a
        client in the crashed process's place can still drain the journal. The emitted event says
        `status="error"` with an explicit unsettled reason — never `success`, never a fabricated
        zero-token, zero-cost call, because nothing reported an outcome (the world stopped).
        """
        sent = 0
        try:
            records = self.journal.recover()
        except Exception:
            return 0
        for rec in records:
            try:
                self.track(
                    rec.get("p") or "unknown",
                    rec.get("m"),
                    name=rec.get("n"),
                    operation=rec.get("op"),
                    status="error",
                    error=unsettled_error(rec),
                    trace_id=rec.get("tr"),
                    span_id=rec.get("sp"),
                    parent_span_id=rec.get("ps"),
                    tags=[RECOVERED_TAG],
                    project=rec.get("pj"),
                )
                sent += 1
            except Exception:
                continue
        if sent:
            self.diag.warn(
                "unsettled-spans",
                f"recovered {sent} call(s) that began but never reported an outcome (a previous "
                f"process was killed mid-call). They are recorded with status=error and the "
                f"'{RECOVERED_TAG}' tag; their token counts and cost are unknown, not zero.",
            )
        return sent

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

    def admit(self, name: Optional[str] = None, event_id: Optional[str] = None) -> Admit:
        """Would a call be admitted right now? Pure and instant — decided from the last ingest
        response this client saw, with no round trip (see `admission.py`)."""
        return self.limits.admit(name=name, event_id=event_id)

    def gate(self, name: Optional[str] = None) -> None:
        """The enforcement gate the instrumentation wrappers call before a provider call.

        Raises `BudgetExceeded` under `enforce="block"`, warns under `"warn"`, and is a no-op under
        `"off"`. A stale view kicks off one background refresh and still admits — the decision never
        waits on the network.
        """
        if self.enforce == "off" or not self.enabled:
            return
        # The server mints the event id, so the client cannot know it in advance: this is a fresh
        # ticket per call. The shed *rate* therefore matches the server's; the shed *set* does not.
        verdict = self.limits.admit(name=name, event_id=uuid.uuid4().hex)
        if verdict.stale:
            self._refresh_limits_async()
        if verdict.ok:
            return
        if self.record_blocked:
            self._record_blocked(name, verdict)
        msg = f"LightTrack: {name or 'this call'} refused before it was made ({verdict.reason})"
        if self.enforce == "warn":
            self.diag.warn("budget", f'{msg}. enforce="warn", so the call is proceeding anyway.')
            return
        raise BudgetExceeded(verdict, 'Use enforce="warn" to log instead of raising.')

    def _record_blocked(self, name: Optional[str], verdict: Admit) -> None:
        """Record a call this client refused: real traffic, zero usage, and explicitly not spend."""
        self.track(
            "lighttrack", "blocked", name=name, status="error",
            error=f"blocked locally by pre-spend admission ({verdict.reason})",
            tags=[BLOCKED_TAG],
            metadata={"lt_admit_reason": verdict.reason,
                      "lt_retry_after_secs": verdict.retry_after_secs},
        )

    def refresh_limits(self) -> None:
        """Refresh the limit view from `GET /v1/limits/status`. Best-effort: a failure leaves the
        old view in place (fail open)."""
        try:
            q = f"?project={urllib.parse.quote(self.project)}" if self.project else ""
            body = self._request("GET", f"/v1/limits/status{q}")
            view = view_from_statuses(body.get("statuses"))
            if view is not None:
                self.limits.observe(view)
        except Exception:
            # An unreachable status endpoint must not change what the app does.
            pass

    def _refresh_limits_async(self) -> None:
        """One poll per stale burst, off the caller's thread."""
        if not self._refresh_lock.acquire(blocking=False):
            return

        def run() -> None:
            try:
                self.refresh_limits()
            finally:
                self._refresh_lock.release()

        threading.Thread(target=run, name="lighttrack-limits", daemon=True).start()

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
        # An orderly shutdown retires its own breadcrumbs; the stale window is the price of *crash*
        # detection and there is no reason to pay it on a clean exit.
        self.journal.close()

    def __enter__(self) -> "LightTrack":
        return self

    def __exit__(self, *exc) -> bool:
        self.close()
        return False

    # ---- internals ----
    def _emit(self, body: dict, path: str = "/v1/events") -> None:
        self._preflight(body)
        if self._async:
            try:
                self._q.put_nowait((path, body))
            except queue.Full:
                # Drop rather than block the caller — but say so, or a saturated queue silently
                # deletes telemetry and looks exactly like "the app made no LLM calls".
                self.diag.warn(
                    "queue-full",
                    f"queue is full ({self._q.maxsize} pending) — dropping events. The LightTrack "
                    "server is slow or unreachable; raise max_queue if this is bursty traffic.",
                )
        else:
            self._post(path, body)

    def _preflight(self, body: dict) -> None:
        """Catch the misconfiguration that is guaranteed to fail *before* spending a round trip on it:
        no project and no API key means the server has no way to attribute the event and will 400."""
        if not body.get("project_id") and not self.api_key:
            self.diag.warn("no-project", no_project_message(self.base_url))

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
            body = e.read().decode("utf-8", "replace")
            raise RelayError(
                f"{method} {path} -> HTTP {e.code}: {body}",
                code=error_code(body),
                status=e.code,
            ) from e
        except Exception as e:
            raise RelayError(f"{method} {path} failed: {e}") from e

    def _post(self, path: str, body: dict) -> None:
        """Best-effort POST: never raises into the host app, but reports what went wrong on stderr
        (rate-limited per error kind) so a failing pipeline is discoverable rather than silent."""
        ctx = {"has_project": bool(body.get("project_id")), "has_key": bool(self.api_key)}
        try:
            data = json.dumps(body).encode("utf-8")
            headers = {"Content-Type": "application/json"}
            if self.api_key:
                headers["Authorization"] = f"Bearer {self.api_key}"
            req = urllib.request.Request(f"{self.base_url}{path}", data=data, headers=headers,
                                         method="POST")
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                # Every ingest response, accepted or refused, is evidence about the project's
                # position. Reading it here is what makes `admit()` answer from the wall the app is
                # actually near, rather than from a poll it never makes.
                self._observe_limits(resp.status, resp.headers, resp.read())
        except urllib.error.HTTPError as e:
            body = _error_body(e)
            self._observe_limits(e.code, e.headers, body)
            self.diag.warn(diagnostic_kind(e.code), send_failure_message(
                self.base_url, path, f"HTTP {e.code} {body}", status=e.code, **ctx))
        except Exception as e:
            # urlopen surfaces a timeout either directly or wrapped in URLError.reason, so both are
            # checked: a timeout bucketed as "network" hides behind (and crowds out) real connection
            # failures, which is exactly the confusion the per-kind rate limiter exists to avoid.
            timed_out = isinstance(e, TimeoutError) or isinstance(getattr(e, "reason", None), TimeoutError)
            self.diag.warn(diagnostic_kind(timed_out=timed_out), send_failure_message(
                self.base_url, path, f"{type(e).__name__}: {e}", **ctx))

    def _observe_limits(self, status: int, headers: Any, raw: Any) -> None:
        """Fold one ingest response into the admission cache. Never raises into the send path."""
        try:
            if isinstance(raw, (bytes, bytearray)):
                raw = raw.decode("utf-8", "replace")
            try:
                body = json.loads(raw) if raw else None
            except Exception:
                # A proxy's HTML, a truncated stream — the status and headers still carry the signal.
                body = None
            self.limits.observe(parse_limit_view(status, dict(headers.items()) if headers else None,
                                                 body))
        except Exception:
            pass


class Span:
    """A timing context manager that tracks one call on exit (latency measured automatically).

    The event is still sent on exit — that is when the usage and the outcome are known. What
    changed: `__enter__` also writes a crash-surviving breadcrumb (see `journal.py`), so a process
    killed between enter and exit no longer erases the call entirely. Exit retires the breadcrumb.
    """

    def __init__(self, client: LightTrack, provider: str, model: Optional[str], **kw):
        self._c = client
        self._provider = provider
        self._model = model
        self._kw = kw
        self._usage = {"input_tokens": 0, "output_tokens": 0, "cached_input": None}
        self._t0: Optional[float] = None
        self._jkey: Optional[int] = None

    def __enter__(self) -> "Span":
        self._t0 = time.perf_counter()
        self._jkey = self._c.journal.begin({
            "p": self._provider,
            "m": self._model,
            "n": self._kw.get("name"),
            "op": self._kw.get("operation"),
            "tr": self._kw.get("trace_id"),
            "sp": self._kw.get("span_id"),
            "ps": self._kw.get("parent_span_id"),
            "pj": self._kw.get("project"),
        })
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
        try:
            self._c.track(
                self._provider, self._model, latency_ms=latency,
                status="error" if exc_type else None,
                error=str(exc) if exc else None,
                input_tokens=self._usage["input_tokens"], output_tokens=self._usage["output_tokens"],
                cached_input=self._usage["cached_input"], **self._kw,
            )
        finally:
            # The breadcrumb is retired on EVERY exit path — success, provider error, cancellation.
            # An unretired breadcrumb would resurface later as a phantom unsettled call, which is a
            # measuring instrument lying in the other direction.
            self._c.journal.settle(self._jkey)
            self._jkey = None
        return False  # never suppress the caller's exception
