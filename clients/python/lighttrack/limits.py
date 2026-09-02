"""Read what an ingest response says about the project's limits.

Parsing only. What a client *does* with the answer — pausing before it spends, honouring the
schedule, shedding its own traffic — is pre-spend admission, and lives elsewhere. Splitting the two
matters because the reading is the part that must be identical in every SDK: the same bytes have to
mean the same thing in Python, TypeScript and Rust, or a fleet mixing them enforces three different
policies. The cases are fixed in `clients/contract/fixtures/limits.json`.

The recurring trap this closes is `None` vs `0`. A project with no limits reports no ratio at all; a
client that read the absence as `0.0` would believe it had infinite headroom. An unparseable
`Retry-After` is likewise unknown, not "retry immediately".
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Mapping, Optional


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


def parse_limit_view(status: int, headers: Optional[Mapping[str, str]] = None,
                     body: Any = None) -> LimitView:
    """Parse one ingest response into a :class:`LimitView`.

    Pure and total: any shape of body, including none at all, yields a view rather than an exception.
    """
    obj = body if isinstance(body, dict) else {}
    err = obj.get("error") if isinstance(obj.get("error"), dict) else {}
    raw = _header(headers, "retry-after")
    # Deliberately integer-only: `Retry-After` may also be an HTTP-date, and a half-parsed date that
    # came out as 0 would tell the client to hammer the endpoint it was just asked to back off from.
    retry = int(raw.strip()) if raw is not None and raw.strip().isdigit() else None
    code = err.get("code")

    return LimitView(
        accepted=200 <= status < 300,
        rate_limited=status == 429,
        usage_ratio=_number(obj.get("usage_ratio")),
        shed_fraction=_number(obj.get("shed_fraction")),
        retry_after_secs=retry,
        error_code=code if isinstance(code, str) else None,
    )
