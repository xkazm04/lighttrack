"""Read what an ingest response says about the project's limits.

Parsing only. What a client *does* with the answer — pausing before it spends, honouring the
schedule, shedding its own traffic — is pre-spend admission, and lives in `admission.py`. Splitting
the two matters because the reading is the part that must be identical in every SDK: the same bytes
have to mean the same thing in Python, TypeScript and Rust, or a fleet mixing them enforces three
different policies. The cases are fixed in `clients/contract/fixtures/limits.json`.

The recurring trap this closes is `None` vs `0`. A project with no limits reports no ratio at all; a
client that read the absence as `0.0` would believe it had infinite headroom. An unparseable
`Retry-After` is likewise unknown, not "retry immediately".

Signals arrive on two channels. ``POST /v1/events`` carries them as body fields. The batch door
answers multi-status (the project's position is not a property of item 7) and the OTLP door answers
in the exporter's own envelope, so neither has a body field to put them in — both send
``X-LightTrack-Usage-Ratio`` / ``-Shed-Fraction`` / ``-Retry-After`` instead, and so does the 429,
which has no ``IngestResponse`` body at all. The body wins where both are present.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Mapping, Optional


@dataclass
class BindingScope:
    """The dimension the binding rule applies to. `None` on a view means project-wide."""

    #: `provider` | `model` | `name` | `api_key` | `customer`.
    kind: str
    value: str


@dataclass
class LimitView:
    """What an ingest response says about limits. Every unknown is `None`, never a stand-in value."""

    #: The event was recorded (2xx).
    accepted: bool
    #: Refused for a usage limit (429) — a hard cap or graduated shedding.
    rate_limited: bool
    #: Worst usage ratio among the rules that applied; `1.0` is at the cap. `None` when unknown.
    usage_ratio: Optional[float] = None
    #: Share of ingest currently being shed, 0.0–1.0. `None` when nothing is throttling.
    shed_fraction: Optional[float] = None
    #: Seconds to wait, from `Retry-After`. `None` when absent or not a number (e.g. an HTTP-date).
    retry_after_secs: Optional[int] = None
    #: The API's stable error code (`rate_limited`, `bad_request`, …). `None` on success.
    error_code: Optional[str] = None
    #: Which rule the ratio belongs to; `None` = project-wide (or unknown). `0.94` alone says stop
    #: everything, `0.94` on `model=gpt-4o` says route the next call elsewhere and keep working.
    binding_scope: Optional[BindingScope] = None
    #: Id of the binding rule. The server's shed decision is a hash of `(rule_id, event_id)`, so
    #: this is what lets a client reproduce it rather than merely run the same function.
    binding_rule: Optional[str] = None


def _number(v: Any) -> Optional[float]:
    # `bool` is an `int` in Python; a JSON `true` must not read as the ratio 1.0.
    if isinstance(v, bool) or not isinstance(v, (int, float)):
        return None
    return float(v)


def _header(headers: Optional[Mapping[str, str]], name: str) -> Optional[str]:
    """Header lookup that does not care about casing — HTTP does not guarantee it, proxies rewrite it."""
    if not headers:
        return None
    want = name.lower()
    for k, v in headers.items():
        if str(k).lower() == want:
            return v
    return None


def _header_number(headers: Optional[Mapping[str, str]], name: str) -> Optional[float]:
    raw = _header(headers, name)
    if raw is None:
        return None
    try:
        return float(raw.strip())
    except ValueError:
        return None


def _retry_after(raw: Optional[str]) -> Optional[int]:
    # Deliberately integer-only: `Retry-After` may also be an HTTP-date, and a half-parsed date that
    # came out as 0 would tell the client to hammer the endpoint it was just asked to back off from.
    return int(raw.strip()) if raw is not None and raw.strip().isdigit() else None


def _binding_scope(v: Any) -> Optional[BindingScope]:
    if not isinstance(v, dict):
        return None
    kind, value = v.get("kind"), v.get("value")
    if isinstance(kind, str) and isinstance(value, str):
        return BindingScope(kind=kind, value=value)
    return None


def parse_limit_view(status: int, headers: Optional[Mapping[str, str]] = None,
                     body: Any = None) -> LimitView:
    """Parse one ingest response into a :class:`LimitView`.

    Pure and total: any shape of body, including none at all, yields a view rather than an exception.
    """
    obj = body if isinstance(body, dict) else {}
    err = obj.get("error") if isinstance(obj.get("error"), dict) else {}
    # The standard header is the contract; the `X-LightTrack-` mirror is the copy that survives a
    # proxy which dropped the original. Never the other way round.
    retry = _retry_after(_header(headers, "retry-after"))
    if retry is None:
        retry = _retry_after(_header(headers, "x-lighttrack-retry-after"))
    code = err.get("code")
    rule = obj.get("binding_rule")

    ratio = _number(obj.get("usage_ratio"))
    if ratio is None:
        ratio = _header_number(headers, "x-lighttrack-usage-ratio")
    shed = _number(obj.get("shed_fraction"))
    if shed is None:
        shed = _header_number(headers, "x-lighttrack-shed-fraction")

    return LimitView(
        accepted=200 <= status < 300,
        rate_limited=status == 429,
        usage_ratio=ratio,
        shed_fraction=shed,
        retry_after_secs=retry,
        error_code=code if isinstance(code, str) else None,
        binding_scope=_binding_scope(obj.get("binding_scope")),
        binding_rule=rule if isinstance(rule, str) else None,
    )
