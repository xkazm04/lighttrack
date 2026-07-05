"""One-line auto-instrumentation for the official OpenAI / Anthropic / Gemini SDKs.

Monkey-patch the provider clients so *every* call is captured automatically — model, token usage,
latency, and full trace linkage — instead of hand-writing a `track*` per call. Two entry points:

    import lighttrack.auto                  # zero-config: patch every installed SDK globally

    from lighttrack import wrap
    client = wrap(OpenAI())                  # explicit: instrument one client instance (drop-in)

Trace context is propagated with `contextvars`: calls inside a `with trace():` block share one
`trace_id`, and calls inside a nested `with span():` get its id as their `parent_span_id`, so they
feed straight into the trace view. A call with no active trace becomes its own single-span trace.

Best-effort, like the rest of the client: instrumentation never breaks the host call. The provider's
own exception still propagates (and is recorded as a failed span); telemetry failures are swallowed.
"""

from __future__ import annotations

import contextlib
import contextvars
import inspect
import time
import uuid
from typing import Any, Callable, List, Optional, Tuple

from .client import LightTrack, _extract_anthropic, _extract_gemini, _extract_openai

Extract = Callable[[Any], Tuple[Optional[str], int, int, Optional[int]]]

# ---- trace context ---------------------------------------------------------

_trace_id: "contextvars.ContextVar[Optional[str]]" = contextvars.ContextVar(
    "lighttrack_trace_id", default=None
)
_parent_span: "contextvars.ContextVar[Optional[str]]" = contextvars.ContextVar(
    "lighttrack_parent_span", default=None
)


def _new_id() -> str:
    return uuid.uuid4().hex


def current_trace_id() -> Optional[str]:
    """The trace id auto-instrumented calls will use right now (None if no trace is open)."""
    return _trace_id.get()


def current_span_id() -> Optional[str]:
    """The span id auto-instrumented calls will attach to as their parent (None at trace root)."""
    return _parent_span.get()


@contextlib.contextmanager
def trace(trace_id: Optional[str] = None):
    """Open a trace: every auto-instrumented call inside shares this `trace_id`. Nestable; a fresh
    trace resets the parent span. Yields the resolved trace id."""
    tok = _trace_id.set(trace_id or _new_id())
    ptok = _parent_span.set(None)
    try:
        yield _trace_id.get()
    finally:
        _parent_span.reset(ptok)
        _trace_id.reset(tok)


@contextlib.contextmanager
def span(span_id: Optional[str] = None):
    """Open a logical span: auto-instrumented calls inside link to it via `parent_span_id` (so agent
    steps nest in the trace tree). Starts a trace if none is open. Yields the span id."""
    sid = span_id or _new_id()
    ttok = _trace_id.set(_trace_id.get() or _new_id())
    ptok = _parent_span.set(sid)
    try:
        yield sid
    finally:
        _parent_span.reset(ptok)
        _trace_id.reset(ttok)


# ---- default client --------------------------------------------------------

_default: Optional[LightTrack] = None


def _get_default_client() -> LightTrack:
    global _default
    if _default is None:
        _default = LightTrack()
    return _default


# ---- call tracking ---------------------------------------------------------

def _ms(t0: float) -> int:
    return int((time.perf_counter() - t0) * 1000)


def _record(lt: LightTrack, provider: str, extract: Extract, operation: str,
            fallback_model: Optional[str], latency_ms: int, resp: Any, error: Optional[BaseException]) -> None:
    """Emit one span for a (possibly failed) provider call. Never raises."""
    try:
        model, inp, out, cached = fallback_model, 0, 0, None
        if resp is not None:
            m, inp, out, cached = extract(resp)
            model = m or fallback_model
        lt.track(
            provider, model,
            input_tokens=inp, output_tokens=out, cached_input=cached,
            operation=operation, latency_ms=latency_ms,
            status="error" if error is not None else None,
            error=str(error) if error is not None else None,
            trace_id=current_trace_id() or _new_id(),
            span_id=_new_id(),
            parent_span_id=current_span_id(),
        )
    except Exception:
        pass  # instrumentation must never break the host app


def _fallback_model(args: tuple, kwargs: dict, instance: Any) -> Optional[str]:
    return (
        kwargs.get("model")
        or getattr(instance, "model_name", None)  # google.generativeai.GenerativeModel
        or getattr(instance, "model", None)
    )


def _wrap(lt: LightTrack, fn: Callable, provider: str, extract: Extract, operation: str,
          *, takes_self: bool) -> Callable:
    """Wrap `fn` (an unbound class method when `takes_self`, else a bound instance method) so each call
    is timed and tracked. Preserves sync vs. async."""
    is_async = inspect.iscoroutinefunction(fn)

    def fallback(args: tuple, kwargs: dict) -> Optional[str]:
        instance = args[0] if takes_self and args else None
        call_args = args[1:] if takes_self else args
        return _fallback_model(call_args, kwargs, instance)

    if is_async:
        async def awrapper(*args: Any, **kwargs: Any) -> Any:
            t0 = time.perf_counter()
            try:
                resp = await fn(*args, **kwargs)
            except BaseException as e:  # noqa: BLE001 - record then re-raise
                _record(lt, provider, extract, operation, fallback(args, kwargs), _ms(t0), None, e)
                raise
            _record(lt, provider, extract, operation, fallback(args, kwargs), _ms(t0), resp, None)
            return resp

        wrapper: Callable = awrapper
    else:
        def wrapper(*args: Any, **kwargs: Any) -> Any:  # type: ignore[misc]
            t0 = time.perf_counter()
            try:
                resp = fn(*args, **kwargs)
            except BaseException as e:  # noqa: BLE001 - record then re-raise
                _record(lt, provider, extract, operation, fallback(args, kwargs), _ms(t0), None, e)
                raise
            _record(lt, provider, extract, operation, fallback(args, kwargs), _ms(t0), resp, None)
            return resp

    wrapper._lighttrack_wrapped = True  # type: ignore[attr-defined]
    wrapper._lighttrack_original = fn  # type: ignore[attr-defined]
    return wrapper


# ---- class-level patching (global instrument) ------------------------------

_PATCHED: List[Tuple[Any, str, Callable]] = []


def _patch_class(cls: Any, attr: str, lt: LightTrack, provider: str, extract: Extract, operation: str) -> None:
    original = getattr(cls, attr, None)
    if not callable(original) or getattr(original, "_lighttrack_wrapped", False):
        return
    wrapped = _wrap(lt, original, provider, extract, operation, takes_self=True)
    try:
        setattr(cls, attr, wrapped)
        _PATCHED.append((cls, attr, original))
    except Exception:
        pass


def _patch_instance(obj: Any, attr: str, lt: LightTrack, provider: str, extract: Extract, operation: str) -> None:
    if obj is None:
        return
    bound = getattr(obj, attr, None)
    if not callable(bound) or getattr(bound, "_lighttrack_wrapped", False):
        return
    wrapped = _wrap(lt, bound, provider, extract, operation, takes_self=False)
    try:
        setattr(obj, attr, wrapped)
    except Exception:
        pass


def _instrument_openai(lt: LightTrack) -> None:
    for mod, names, op in (
        ("openai.resources.chat.completions", ("Completions", "AsyncCompletions"), "chat"),
        ("openai.resources.responses", ("Responses", "AsyncResponses"), "chat"),
        ("openai.resources.embeddings", ("Embeddings", "AsyncEmbeddings"), "embedding"),
    ):
        try:
            m = __import__(mod, fromlist=list(names))
            for n in names:
                _patch_class(getattr(m, n), "create", lt, "openai", _extract_openai, op)
        except Exception:
            pass


def _instrument_anthropic(lt: LightTrack) -> None:
    try:
        m = __import__("anthropic.resources.messages", fromlist=["Messages", "AsyncMessages"])
        for n in ("Messages", "AsyncMessages"):
            _patch_class(getattr(m, n), "create", lt, "anthropic", _extract_anthropic, "chat")
    except Exception:
        pass


def _instrument_gemini(lt: LightTrack) -> None:
    try:  # new SDK: google-genai
        m = __import__("google.genai.models", fromlist=["Models", "AsyncModels"])
        for n in ("Models", "AsyncModels"):
            _patch_class(getattr(m, n), "generate_content", lt, "google", _extract_gemini, "chat")
    except Exception:
        pass
    try:  # legacy SDK: google-generativeai
        m = __import__("google.generativeai", fromlist=["GenerativeModel"])
        _patch_class(m.GenerativeModel, "generate_content", lt, "google", _extract_gemini, "chat")
    except Exception:
        pass


_PROVIDERS = {"openai": _instrument_openai, "anthropic": _instrument_anthropic, "google": _instrument_gemini}
_PROVIDER_ALIASES = {"gemini": "google", "claude": "anthropic", "oai": "openai"}


def instrument(lt: Optional[LightTrack] = None, *, providers: Optional[List[str]] = None) -> LightTrack:
    """Monkey-patch the installed provider SDK classes so every client auto-tracks. Idempotent and
    best-effort (missing SDKs are skipped). `providers` optionally restricts the set. Returns the
    client used (the env-configured default unless one is passed)."""
    lt = lt or _get_default_client()
    want = providers or list(_PROVIDERS)
    for p in want:
        fn = _PROVIDERS.get(_PROVIDER_ALIASES.get(p.lower(), p.lower()))
        if fn:
            fn(lt)
    return lt


def uninstrument() -> None:
    """Restore every class method patched by `instrument` (chiefly for tests)."""
    while _PATCHED:
        cls, attr, original = _PATCHED.pop()
        try:
            setattr(cls, attr, original)
        except Exception:
            pass


# ---- instance-level wrapping (explicit, safest) ----------------------------

def wrap(client: Any, lt: Optional[LightTrack] = None) -> Any:
    """Auto-instrument a single OpenAI / Anthropic / Gemini SDK client *instance*, leaving every other
    client untouched. Returns the same object so it stays a drop-in: `client = wrap(OpenAI())`."""
    lt = lt or _get_default_client()
    chat = getattr(client, "chat", None)
    if chat is not None:  # openai
        _patch_instance(getattr(chat, "completions", None), "create", lt, "openai", _extract_openai, "chat")
    _patch_instance(getattr(client, "responses", None), "create", lt, "openai", _extract_openai, "chat")
    _patch_instance(getattr(client, "embeddings", None), "create", lt, "openai", _extract_openai, "embedding")
    _patch_instance(getattr(client, "messages", None), "create", lt, "anthropic", _extract_anthropic, "chat")
    _patch_instance(getattr(client, "models", None), "generate_content", lt, "google", _extract_gemini, "chat")
    if hasattr(client, "model_name"):  # legacy google.generativeai.GenerativeModel
        _patch_instance(client, "generate_content", lt, "google", _extract_gemini, "chat")
    return client


auto_instrument = instrument
