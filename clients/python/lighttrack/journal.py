"""Crash-surviving breadcrumbs for calls that are still in flight.

The defect this closes: every path in this client emitted an event only *after* the provider call
returned — `Span` in `__exit__`, the auto-instrument wrapper after `fn()` — so the coverage profile
was exactly inverted. Orderly completions were recorded perfectly; a process killed mid-call (OOM
killer, SIGKILL, a container eviction, a crashed worker) left **no record at all** of a call that
definitely happened and definitely cost money. For an observability product that silently drops
exactly the runs an operator most needs to reconstruct.

The fix is the one the discipline names: **durability must not be conditioned on the run ending the
way the writer expects.** A span becomes durable when it OPENS, not when it closes.

Why a local journal rather than a span-open POST to the API. The obvious shape — write the open
record to the server, update it on settle — is not available here without corrupting accounting: the
server's ingest is an insert keyed on event id (a duplicate is a 409 by design), and the SQLite
backend's rolling-usage cache for limit admission folds committed rows by `rowid` and cannot observe
an in-place update, so a settle-by-update would make caps silently wrong. A local append-only
journal buys the same crash coverage with no server change and no accounting risk. Its honest limit
is written down rather than glossed: recovery happens when a LightTrack client next starts **with
the same journal directory** — the same box, or a mounted volume. A pod that dies and is rescheduled
onto fresh storage is not covered, and that is the boundary of this mechanism.

Durability level: each record is written and `flush()`ed to the OS. That survives the *process*
dying, which is the case in scope. It does not `fsync`, so a machine losing power can lose the tail
— paying an fsync per span boundary would put a disk round trip on the call path this client
promises never to block.

Everything here is best-effort and never raises into the host app; a journal that broke a program
would be worse than the gap it closes.
"""

from __future__ import annotations

import json
import os
import tempfile
import threading
import time
import uuid
from typing import Any, Dict, List, Optional

#: Journal files are named so a sweep can recognize them without reading them.
FILE_PREFIX = "lighttrack-spans-"
FILE_SUFFIX = ".jsonl"

#: How long a journal file belonging to ANOTHER process must have been untouched before this process
#: treats its open records as orphaned. It is a liveness heuristic, deliberately chosen over a pid
#: check: `os.kill(pid, 0)` is not portable (on Windows `os.kill` terminates rather than probes) and
#: a pid is reused, so a stale journal could be judged live by a brand-new unrelated process.
#:
#: A live client touches its own file on every span open and close, so a busy process is never
#: mistaken for a dead one. The exposure is a single call that stays open longer than this window: it
#: gets reported as unsettled while it is in fact still running. That report is honest either way —
#: a call in flight for five minutes is a fact an operator wants — and the window is configurable for
#: workloads where it is not.
DEFAULT_ORPHAN_SECS = 300.0

#: Tag on every event reconstructed from a journal, so these are filterable and never silently mixed
#: with calls whose outcome was actually observed.
RECOVERED_TAG = "lighttrack:unsettled-span"

_TRUTHY = ("1", "true", "yes", "on")


def _enabled_from_env() -> bool:
    v = (os.environ.get("LIGHTTRACK_JOURNAL") or "").strip().lower()
    return v not in ("0", "false", "no", "off")


def _orphan_secs_from_env() -> float:
    try:
        return float(os.environ.get("LIGHTTRACK_JOURNAL_ORPHAN_SECS") or DEFAULT_ORPHAN_SECS)
    except (TypeError, ValueError):
        return DEFAULT_ORPHAN_SECS


def default_dir() -> str:
    return os.environ.get("LIGHTTRACK_JOURNAL_DIR") or os.path.join(
        tempfile.gettempdir(), "lighttrack-spans"
    )


class SpanJournal:
    """One append-only journal per client instance.

    Records are `{"o": "b", "k": <key>, ...fields}` on open and `{"o": "e", "k": <key>}` on settle.
    A file's unsettled set is its opens minus its closes — a form that needs no rewriting on the hot
    path and is readable even when the last line was truncated by the kill.
    """

    def __init__(
        self,
        *,
        enabled: Optional[bool] = None,
        directory: Optional[str] = None,
        orphan_after: Optional[float] = None,
    ) -> None:
        self.enabled = _enabled_from_env() if enabled is None else bool(enabled)
        self.dir = directory or default_dir()
        self.orphan_after = _orphan_secs_from_env() if orphan_after is None else float(orphan_after)
        self.path: Optional[str] = None
        self._fh: Any = None
        self._lock = threading.Lock()
        self._next_key = 0
        self._open_keys: set = set()
        self._broken = False

    # ---- the hot path ----
    def begin(self, fields: Dict[str, Any]) -> Optional[int]:
        """Record that a call has STARTED. Returns a token to pass to `settle`, or None when the
        journal is off or unusable — callers treat None as "nothing to settle"."""
        if not self.enabled or self._broken:
            return None
        try:
            with self._lock:
                fh = self._handle()
                if fh is None:
                    return None
                self._next_key += 1
                key = self._next_key
                rec = dict(fields)
                rec["o"] = "b"
                rec["k"] = key
                rec.setdefault("t", time.time())
                fh.write(json.dumps(rec, default=str) + "\n")
                fh.flush()
                self._open_keys.add(key)
                return key
        except Exception:
            self._broken = True  # one failure disables it; telemetry never fights the filesystem
            return None

    def settle(self, key: Optional[int]) -> None:
        """Record that the call finished (however it finished). The event itself is emitted by the
        caller through the normal path; this only retires the breadcrumb."""
        if key is None or not self.enabled or self._broken:
            return
        try:
            with self._lock:
                fh = self._handle()
                if fh is None:
                    return
                fh.write(json.dumps({"o": "e", "k": key}) + "\n")
                fh.flush()
                self._open_keys.discard(key)
                # Nothing in flight ⇒ nothing to recover ⇒ the file can go back to empty. This is
                # what keeps a long-lived process's journal from growing without bound, and it means
                # an idle process leaves a zero-byte file rather than a history of settled calls.
                if not self._open_keys:
                    fh.seek(0)
                    fh.truncate()
        except Exception:
            self._broken = True

    def close(self) -> None:
        """Release the journal. A file with nothing in flight is deleted — an orphan sweep should
        find only real orphans."""
        try:
            with self._lock:
                fh, self._fh = self._fh, None
                if fh is not None:
                    fh.close()
                if self.path and not self._open_keys and os.path.exists(self.path):
                    os.remove(self.path)
        except Exception:
            pass

    # ---- recovery ----
    def recover(self) -> List[Dict[str, Any]]:
        """Sweep the journal directory for OTHER processes' abandoned files and return their
        unsettled open records. Each returned file is removed, so a record is reported once.

        Never raises: an unreadable or half-written file yields whatever parsed and is dropped.
        """
        if not self.enabled:
            return []
        out: List[Dict[str, Any]] = []
        try:
            names = os.listdir(self.dir)
        except OSError:
            return out
        now = time.time()
        for name in names:
            if not (name.startswith(FILE_PREFIX) and name.endswith(FILE_SUFFIX)):
                continue
            full = os.path.join(self.dir, name)
            if self.path and os.path.abspath(full) == os.path.abspath(self.path):
                continue  # our own live journal
            try:
                if now - os.path.getmtime(full) < self.orphan_after:
                    continue  # recently written ⇒ assume a live process owns it
                out.extend(_unsettled(full))
                os.remove(full)
            except OSError:
                continue
        return out

    # ---- internals ----
    def _handle(self) -> Any:
        if self._fh is not None:
            return self._fh
        os.makedirs(self.dir, exist_ok=True)
        name = "{}{}-{}{}".format(FILE_PREFIX, os.getpid(), uuid.uuid4().hex[:8], FILE_SUFFIX)
        self.path = os.path.join(self.dir, name)
        # "w+" (not "a"): this file is ours alone, and `settle` truncates it back to empty when
        # nothing is in flight, which append mode cannot do.
        self._fh = open(self.path, "w+", encoding="utf-8")
        return self._fh


def _unsettled(path: str) -> List[Dict[str, Any]]:
    """The open records in one journal file that never got a matching close."""
    opens: Dict[Any, Dict[str, Any]] = {}
    with open(path, "r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except ValueError:
                # A kill mid-write leaves a partial last line. Everything before it is still good;
                # dropping only the torn record is the point of a line-per-record journal.
                continue
            if not isinstance(rec, dict):
                continue
            if rec.get("o") == "b":
                opens[rec.get("k")] = rec
            elif rec.get("o") == "e":
                opens.pop(rec.get("k"), None)
    return list(opens.values())


def unsettled_error(rec: Dict[str, Any]) -> str:
    """The `error` string an unsettled call is reported with. It says what is known (the call began,
    at this time) and what is not (how it ended), rather than presenting a guess as an outcome."""
    started = rec.get("t")
    when = ""
    if isinstance(started, (int, float)):
        when = " started {}Z".format(
            time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(started))
        )
    return (
        "unsettled span: the process that made this call exited or stalled before it reported an "
        "outcome{}. Token counts and cost are unknown, not zero; latency is unknown, not the time "
        "until it was noticed. Recovered from the LightTrack client journal.".format(when)
    )
