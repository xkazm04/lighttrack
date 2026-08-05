"""Make a failed send *visible* without ever making it *throw*.

The SDK is fire-and-forget by contract: telemetry must never break the host app, so `_post` swallows
every exception. Swallowing everything, though, also swallowed the failure every first-time user hits
— follow the README with no project configured, the API answers `400 project_id is required`, the
event vanishes, and nothing at all is printed. The user sees no event and no reason.

So: still never raise, never block, never touch stdout (the host app may be speaking a protocol on
it) — but write one actionable line to **stderr**, rate-limited per error kind so a tight loop of
failing calls prints once rather than thousands of times.

Silence it entirely with `LIGHTTRACK_QUIET=1` (or `LightTrack(quiet=True)`).
"""

from __future__ import annotations

import os
import sys
import threading
import time
from typing import Optional

PREFIX = "[lighttrack]"
#: One line per error kind per this many seconds. A persistent outage still re-warns (with a count of
#: what was suppressed) instead of going quiet forever after the first line.
COOLDOWN_SECS = 60.0
SILENCE_HINT = 'silence these warnings with LIGHTTRACK_QUIET=1 or LightTrack(quiet=True)'
_TRUTHY = ("1", "true", "yes", "on")


def env_quiet() -> bool:
    return (os.environ.get("LIGHTTRACK_QUIET") or "").strip().lower() in _TRUTHY


def truncate(s: str, limit: int = 200) -> str:
    s = " ".join(str(s).split())
    return s if len(s) <= limit else s[: limit - 3] + "..."


class Diagnostics:
    """Rate-limited stderr warner. Every method is exception-proof: a diagnostic must never become
    the failure it is reporting."""

    def __init__(self, quiet: Optional[bool] = None, cooldown: float = COOLDOWN_SECS):
        self.quiet = env_quiet() if quiet is None else bool(quiet)
        self.cooldown = cooldown
        self.emitted = 0  # lines actually written (test hook)
        self.suppressed = 0  # lines withheld by the rate limiter (test hook)
        self._seen: dict = {}  # kind -> (last_emitted_monotonic, suppressed_since)
        self._lock = threading.Lock()

    def warn(self, kind: str, message: str) -> None:
        """Emit `message` at most once per `kind` per cooldown window."""
        try:
            if self.quiet:
                return
            now = time.monotonic()
            with self._lock:
                last, held = self._seen.get(kind, (None, 0))
                if last is not None and now - last < self.cooldown:
                    self._seen[kind] = (last, held + 1)
                    self.suppressed += 1
                    return
                self._seen[kind] = (now, 0)
                self.emitted += 1
                first_line = self.emitted == 1
            repeat = f" [{held} more suppressed in the last {int(self.cooldown)}s]" if held else ""
            hint = f"\n  {PREFIX} {SILENCE_HINT}" if first_line else ""
            print(f"{PREFIX} {message}{repeat}{hint}", file=sys.stderr, flush=True)
        except Exception:
            pass  # a diagnostic must never break the host app either


def no_project_message(base_url: str) -> str:
    """The first-run trap: no project *and* no API key, so the server cannot infer one and will
    reject every event with 400. Detected before the network call, so the user is told immediately.

    Messages stay ASCII-only: they are written to whatever console the host app happens to have, and
    a cp1252 Windows terminal turns a stray em dash into mojibake."""
    return (
        "events are being dropped: no project is configured, and without an API key the server "
        "cannot infer one, so it will reject them with HTTP 400 'project_id is required'. Fix: set "
        "LIGHTTRACK_PROJECT=<your-project-id> (or LightTrack(project='...')), or set LIGHTTRACK_KEY "
        f"to a project API key, which pins the project server-side. Target: {base_url}"
    )


def send_failure_message(base_url: str, path: str, detail: str, *, status: Optional[int] = None,
                         has_project: bool = False, has_key: bool = False) -> str:
    hint = _hint(base_url, status, has_project=has_project, has_key=has_key)
    return f"event not sent to {base_url}{path}: {detail}." + (f" {hint}" if hint else "")


def _hint(base_url: str, status: Optional[int], *, has_project: bool, has_key: bool) -> str:
    if status is None:
        return (f"Is a LightTrack server running and reachable at {base_url}? Check LIGHTTRACK_URL. "
                "Events are dropped while it is unreachable.")
    if status == 400 and not has_project:
        # The same trap as `no_project_message`, reached the slow way: a key was set (an *admin* key,
        # which pins no project) so the preflight check passed and the server did the rejecting.
        return ("The server has no project for this event. Fix: set LIGHTTRACK_PROJECT="
                "<your-project-id> (or LightTrack(project='...')), or use a *project* API key in "
                "LIGHTTRACK_KEY; an admin key does not imply a project.")
    if status == 400:
        return "The event was rejected as invalid: check provider / model / usage."
    if status in (401, 403):
        return ("The key was rejected. Set LIGHTTRACK_KEY to a valid project or admin key "
                "(or LightTrack(api_key='...'))." if has_key
                else "This server requires authentication. Set LIGHTTRACK_KEY to a project API key.")
    if status == 404:
        return f"No such endpoint — is LIGHTTRACK_URL ({url}) pointing at a LightTrack API?"
    if status == 429:
        return "The project is over a configured usage limit, so ingest is being refused."
    if status >= 500:
        return "The LightTrack server errored; events are dropped until it recovers."
    return ""
