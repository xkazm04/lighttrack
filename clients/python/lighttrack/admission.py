"""Pre-spend admission: decide, before the provider call, whether to make it at all.

Every cap LightTrack has is record-side. The server refuses to *record* a call that already cost
money — the money is gone by the time the 429 arrives. The signals to do better were already on the
wire (``usage_ratio``, ``shed_fraction``, ``Retry-After``, and now the ``X-LightTrack-*`` headers);
this module is what finally reads them and acts.

Three rules shape the design:

1. **Pure.** :meth:`AdmissionCache.admit` performs no I/O and reads no clock it was not handed. A
   decision that could block on a network call would put LightTrack on the critical path of every
   LLM call in the host app — precisely the cost ``docs/ARCHITECTURE.md`` §4 deferred the inline
   gateway to avoid.
2. **Fails open.** No observation, or an observation older than the TTL, admits. A telemetry client
   that stops an app's LLM calls because it is itself confused is worse than one that records
   nothing.
3. **Scoped.** A cap on the ``summarize`` use-case must stop ``summarize`` and nothing else. Views
   are cached per binding scope, which is why the server names it.

The verdicts are fixed across all three SDKs in ``clients/contract/fixtures/limits.json``.
"""

from __future__ import annotations

import math
import time
from dataclasses import dataclass
from typing import Any, Dict, Optional

from .limits import BindingScope, LimitView

#: How long a cached view is still evidence. Past it, :meth:`AdmissionCache.admit` admits and says so.
DEFAULT_ADMISSION_TTL_MS = 30_000

_MASK = (1 << 64) - 1
_PROJECT_WIDE = ""


def shed_ticket(rule_id: str, event_id: str) -> float:
    """Map ``(rule, event)`` to a stable point in ``[0, 1)`` — the server's shed lottery (§7c).

    A port of ``lighttrack_core::shed_ticket`` rather than a re-invention: a different hash would
    still shed proportionally and still look right in aggregate, while disagreeing with the server
    on every individual event. The values are pinned in the ``shed_lottery`` fixture, which the Rust
    runner checks against the server's own function.

    Note the multiplier below is the server's own ``0x1000000001B3``, *not* the textbook FNV prime
    ``0x100000001B3``: reaching for the standard constant yields a perfectly good hash that
    disagrees with the server on every single event.
    """
    h = 0xCBF29CE484222325
    for b in rule_id.encode("utf-8") + b"\x1f" + event_id.encode("utf-8"):
        h ^= b
        h = (h * 0x1000000001B3) & _MASK
    # FNV mixes its low bits well and its high ones poorly on short inputs, and we want the top 53.
    h ^= h >> 30
    h = (h * 0xBF58476D1CE4E5B9) & _MASK
    h ^= h >> 27
    h = (h * 0x94D049BB133111EB) & _MASK
    h ^= h >> 31
    return (h >> 11) / float(1 << 53)


@dataclass
class Admit:
    """The verdict on one prospective call."""

    #: Whether the provider call should be made.
    ok: bool
    #: `None` when ok; otherwise `retry_after` | `at_cap` | `shed`.
    reason: Optional[str] = None
    #: Only set for `retry_after` — a client must not invent a back-off the server never promised.
    retry_after_secs: Optional[int] = None
    #: The view is past its TTL, so this verdict was taken without current evidence (and admits).
    stale: bool = False


@dataclass
class _Entry:
    usage_ratio: Optional[float]
    shed_fraction: Optional[float]
    #: Absolute deadline of a 429's advertised wait, in ms, or `None`.
    retry_after_until_ms: Optional[float]
    binding_scope: Optional[BindingScope]
    binding_rule: Optional[str]
    refreshed_at_ms: float


def _now_ms() -> float:
    return time.time() * 1000.0


def _scope_key(scope: Optional[BindingScope]) -> str:
    return f"{scope.kind}={scope.value}" if scope else _PROJECT_WIDE


class AdmissionCache:
    """The per-client store of what the server last said, and the decision taken from it.

    One entry per binding scope: the project-wide view under ``""``, a ``name``-scoped view under
    ``name=<use-case>``, and so on. Nothing is evicted by count — the set of scopes a project's rules
    can name is small and operator-authored.
    """

    def __init__(self, ttl_ms: int = DEFAULT_ADMISSION_TTL_MS):
        self.ttl_ms = ttl_ms
        self._views: Dict[str, _Entry] = {}

    def observe(self, view: LimitView, now_ms: Optional[float] = None) -> None:
        """Fold one parsed ingest response into the cache."""
        now = _now_ms() if now_ms is None else now_ms
        key = _scope_key(view.binding_scope)
        prior = self._views.get(key)
        # Only a 429 arms the wait. A 503 carries `Retry-After` too, but it means the *ingest
        # endpoint* is saturated — pausing the app's LLM calls over that would be the observability
        # tool causing the outage it exists to observe. And a 2xx is the server saying the refusal is
        # over, which outranks a schedule the client is still holding.
        if view.accepted:
            until: Optional[float] = None
        elif view.rate_limited and view.retry_after_secs is not None:
            until = now + view.retry_after_secs * 1000.0
        else:
            until = prior.retry_after_until_ms if prior else None

        self._views[key] = _Entry(
            usage_ratio=view.usage_ratio,
            shed_fraction=view.shed_fraction,
            retry_after_until_ms=until,
            binding_scope=view.binding_scope,
            binding_rule=view.binding_rule,
            refreshed_at_ms=now,
        )

    def clear(self) -> None:
        """Drop everything (a key rotation, a project switch — anything invalidating the evidence)."""
        self._views.clear()

    def admit(self, name: Optional[str] = None, event_id: Optional[str] = None,
              now_ms: Optional[float] = None) -> Admit:
        """Decide one prospective call. Pure: no I/O, and no clock beyond `now_ms`.

        A `name` is answered from that use-case's own view when the server has named one, and from
        the project-wide view otherwise — applying the worst rule in the project to every call is how
        a scoped budget turns into a project-wide outage.
        """
        now = _now_ms() if now_ms is None else now_ms
        entry = self._views.get(f"name={name}") if name else None
        if entry is None:
            entry = self._views.get(_PROJECT_WIDE)
        if entry is None:
            return Admit(ok=True)

        # The advertised wait is an absolute deadline, so it is honoured even past the TTL: the
        # server told us when to come back, and that instruction does not go stale, it expires.
        if entry.retry_after_until_ms is not None and now < entry.retry_after_until_ms:
            return Admit(
                ok=False,
                reason="retry_after",
                retry_after_secs=int(math.ceil((entry.retry_after_until_ms - now) / 1000.0)),
            )
        if now - entry.refreshed_at_ms > self.ttl_ms:
            return Admit(ok=True, stale=True)
        if entry.usage_ratio is not None and entry.usage_ratio >= 1.0:
            return Admit(ok=False, reason="at_cap")
        if (
            entry.shed_fraction
            and entry.shed_fraction > 0
            and event_id is not None
            and shed_ticket(entry.binding_rule or "", event_id) < entry.shed_fraction
        ):
            return Admit(ok=False, reason="shed")
        return Admit(ok=True)


class BudgetExceeded(Exception):
    """The refusal an enforcing wrapper raises instead of making the provider call.

    Typed, because the host app has to be able to tell "your budget said no" from a provider outage:
    the first is a decision it may want to degrade around (a smaller model, a cached answer, a
    queue), the second is a retry.
    """

    def __init__(self, verdict: Admit, detail: str = ""):
        wait = f"; retry in {verdict.retry_after_secs}s" if verdict.retry_after_secs is not None else ""
        super().__init__(
            f"LightTrack refused this call before it was made ({verdict.reason or 'unknown'}){wait}"
            + (f". {detail}" if detail else "")
        )
        self.reason = verdict.reason
        self.retry_after_secs = verdict.retry_after_secs


def view_from_statuses(statuses: Any) -> Optional[LimitView]:
    """Collapse ``GET /v1/limits/status``'s ``statuses`` into one view, as the ingest doors do:
    worst ratio, strongest shed, and the identity of the worst rule."""
    if not isinstance(statuses, list) or not statuses:
        return None
    worst: Any = None
    ratio: Optional[float] = None
    shed: Optional[float] = None
    for s in statuses:
        if not isinstance(s, dict):
            continue
        r = s.get("ratio")
        if isinstance(r, (int, float)) and not isinstance(r, bool) and (ratio is None or r > ratio):
            ratio, worst = float(r), s
        f = s.get("shed_fraction")
        if isinstance(f, (int, float)) and not isinstance(f, bool) and f > 0 and (shed is None or f > shed):
            shed = float(f)
    scope = worst.get("scope") if isinstance(worst, dict) else None
    binding = None
    if isinstance(scope, dict) and scope:
        kind = next(iter(scope))
        value = scope[kind]
        if isinstance(value, str):
            binding = BindingScope(kind=kind, value=value)
    rule = worst.get("rule_id") if isinstance(worst, dict) else None
    return LimitView(
        accepted=True,
        rate_limited=False,
        usage_ratio=ratio,
        shed_fraction=shed,
        binding_scope=binding,
        binding_rule=rule if isinstance(rule, str) else None,
    )
