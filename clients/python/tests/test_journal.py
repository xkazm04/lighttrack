"""The crash-surviving span journal: what a killed process leaves behind, and what a later one does
with it.

Stdlib-only and network-free. The defect under test is the one that mattered most in an
observability client: every emit path fired only *after* the provider call returned, so a process
killed mid-call left no record of a call that definitely happened and definitely cost money. These
tests pin the mechanism that closes it, and — deliberately — also pin the ways it must NOT lie:
a settled call leaves nothing, a live process's journal is not stolen, and a recovered call is
reported as unknown-outcome rather than as a zero-cost success.

Run from `clients/python`:  `python -m unittest discover tests`
"""

import os
import tempfile
import time
import unittest

from lighttrack.client import LightTrack, Span
from lighttrack.journal import RECOVERED_TAG, SpanJournal, unsettled_error


class Captured(LightTrack):
    """A client that captures events instead of sending them, but keeps the real journal."""

    def __init__(self, **kw):
        self.sent = []
        super().__init__(enabled=True, async_=False, **kw)

    def _emit(self, body, path="/v1/events"):
        self.sent.append((path, body))


def _client(dirpath, **kw):
    return Captured(base_url="http://127.0.0.1:1", project="p", quiet=True,
                    journal_dir=dirpath, **kw)


def _kill(client):
    """Simulate the process dying: drop the OS handle on the journal WITHOUT retiring it.

    `close()` is the orderly path — it settles and deletes. A killed process does neither; the
    kernel just reclaims its file descriptor, leaving the journal on disk with open records in it.
    (In-process this also matters mechanically on Windows, where a later sweep cannot unlink a file
    another handle still holds.)
    """
    fh, client.journal._fh = client.journal._fh, None
    if fh is not None:
        fh.close()


class JournalTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = self._tmp.name
        self.addCleanup(self._tmp.cleanup)

    # ---- the defect this exists for ----
    def test_a_span_that_never_exits_is_recovered_by_the_next_client(self):
        """The kill case: a process opens a span and dies. Nothing calls __exit__, so the old client
        emitted nothing at all. Now the breadcrumb outlives the process."""
        crashed = _client(self.dir)
        span = crashed.span("openai", "gpt-4o", name="summarize")
        span.__enter__()  # ... and the process is killed here: no __exit__, no close()
        self.assertEqual(crashed.sent, [], "nothing is emitted while the call is in flight")
        _kill(crashed)

        _age(self.dir)  # the file has to look abandoned, not merely young
        survivor = _client(self.dir)
        events = [b for p, b in survivor.sent if p == "/v1/events"]
        self.assertEqual(len(events), 1, f"the killed call must survive: {survivor.sent}")
        ev = events[0]
        self.assertEqual(ev["provider"], "openai")
        self.assertEqual(ev["model"], "gpt-4o")
        self.assertEqual(ev["name"], "summarize")
        self.assertIn(RECOVERED_TAG, ev["tags"])

    def test_a_recovered_call_reports_an_unknown_outcome_not_a_success(self):
        """Nothing reported an outcome — the world stopped. A recovered call must not read as a
        clean zero-token, zero-cost success, which is the shape a naive replay would produce."""
        crashed = _client(self.dir)
        crashed.span("anthropic", "claude").__enter__()
        _kill(crashed)
        _age(self.dir)

        ev = [b for p, b in _client(self.dir).sent if p == "/v1/events"][0]
        self.assertEqual(ev["status"], "error")
        self.assertIn("unsettled span", ev["error"])
        self.assertNotIn("latency_ms", ev, "latency is unknown, not the time until it was noticed")
        self.assertEqual(ev["usage"], {"input": 0, "output": 0})
        self.assertIn("unknown, not zero", ev["error"], "the zeros must be labelled as unknown")

    # ---- and the ways it must not lie ----
    def test_a_settled_span_leaves_nothing_to_recover(self):
        lt = _client(self.dir)
        with lt.span("openai", "gpt-4o") as s:
            s.set_usage(10, 20)
        lt.close()
        _age(self.dir)

        after = _client(self.dir)
        recovered = [b for p, b in after.sent if RECOVERED_TAG in (b.get("tags") or [])]
        self.assertEqual(recovered, [], "a call whose outcome WAS observed must not be re-reported")

    def test_a_span_that_raises_is_settled_too(self):
        """The provider error path is an exit path. If it did not retire the breadcrumb, every failed
        call would later resurface a second time as a phantom unsettled one."""
        lt = _client(self.dir)
        with self.assertRaises(RuntimeError):
            with lt.span("openai", "gpt-4o"):
                raise RuntimeError("boom")
        lt.close()
        _age(self.dir)
        after = _client(self.dir)
        self.assertEqual([b for p, b in after.sent if RECOVERED_TAG in (b.get("tags") or [])], [])

    def test_a_recent_journal_is_left_alone(self):
        """A second process starting up must not steal a live one's in-flight calls. Freshness is
        the liveness signal: a live client touches its journal on every span boundary."""
        live = _client(self.dir)
        live.span("openai", "gpt-4o").__enter__()
        self.addCleanup(_kill, live)
        # No ageing: the file was written moments ago.
        other = _client(self.dir)
        self.assertEqual([b for p, b in other.sent if RECOVERED_TAG in (b.get("tags") or [])], [])

    def test_recovery_happens_once(self):
        crashed = _client(self.dir)
        crashed.span("openai", "gpt-4o").__enter__()
        _kill(crashed)
        _age(self.dir)
        first = _client(self.dir)
        self.assertEqual(len(first.sent), 1)
        _age(self.dir)
        second = _client(self.dir)
        self.assertEqual(second.sent, [], "a recovered breadcrumb is consumed, not replayed forever")

    def test_a_torn_final_line_does_not_lose_the_records_before_it(self):
        """A kill mid-write leaves a partial last line. One record per line exists precisely so the
        torn one is the only casualty."""
        j = SpanJournal(directory=self.dir)
        j.begin({"p": "openai", "m": "a"})
        j.begin({"p": "openai", "m": "b"})
        with open(j.path, "a", encoding="utf-8") as fh:
            fh.write('{"o": "b", "k": 3, "p": "opena')  # killed mid-write
        j._fh.close()
        j._fh = None
        _age(self.dir)

        found = SpanJournal(directory=self.dir).recover()
        self.assertEqual(sorted(r["m"] for r in found), ["a", "b"])

    def test_disabled_journal_writes_nothing(self):
        j = SpanJournal(enabled=False, directory=self.dir)
        self.assertIsNone(j.begin({"p": "openai"}))
        j.settle(None)
        self.assertEqual(os.listdir(self.dir), [])

    def test_an_unusable_directory_never_breaks_the_caller(self):
        """A journal is telemetry about telemetry. It must degrade, never raise."""
        j = SpanJournal(directory=os.path.join(self.dir, "f", "not-a-dir"))
        with open(os.path.join(self.dir, "f"), "w", encoding="utf-8"):
            pass
        self.assertIsNone(j.begin({"p": "openai"}))
        self.assertEqual(j.recover(), [])

    def test_unsettled_error_names_what_is_known_and_what_is_not(self):
        msg = unsettled_error({"t": 1_700_000_000.0})
        self.assertIn("2023-11-14", msg)
        self.assertIn("exited or stalled", msg)


def _age(dirpath, seconds=10_000):
    """Backdate every journal file so the freshness heuristic treats it as abandoned."""
    old = time.time() - seconds
    for name in os.listdir(dirpath):
        path = os.path.join(dirpath, name)
        if os.path.isfile(path):
            os.utime(path, (old, old))


if __name__ == "__main__":
    unittest.main()
