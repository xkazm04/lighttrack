"""Payload-shape tests for auto-instrument trace/span linkage.

Stdlib-only and network-free: a fake client captures every `track(...)` call's kwargs, so we can
assert the emitted `trace_id` / `span_id` / `parent_span_id` shape without an API. The span tree is
only real if the first call in a span *owns* its id and later calls hang under it — that is what
these tests pin down.

Run from `clients/python`:  `python -m unittest discover tests`
"""

import unittest

from lighttrack.instrument import (
    _record,
    current_span_id,
    current_trace_id,
    span,
    trace,
)


class FakeClient:
    """Captures track(...) kwargs instead of sending anything."""

    def __init__(self):
        self.calls = []

    def track(self, provider, model, **kw):
        self.calls.append(kw)


def rec(lt, model="m"):
    # resp=None -> extract is never called; we assert linkage, not usage.
    _record(lt, "openai", lambda r: (None, 0, 0, None), "chat", model, 5, None, None)


class SpanLinkageTests(unittest.TestCase):
    def test_call_without_span_is_standalone_root(self):
        lt = FakeClient()
        rec(lt)
        c = lt.calls[0]
        self.assertIsNotNone(c["span_id"])
        self.assertIsNone(c["parent_span_id"])
        self.assertIsNotNone(c["trace_id"])

    def test_first_call_owns_span_rest_are_children(self):
        lt = FakeClient()
        with span("S1"):
            rec(lt)
            rec(lt)
            rec(lt)
        owner, second, third = lt.calls
        # First call claims the span id, parenting to the (absent) enclosing span.
        self.assertEqual(owner["span_id"], "S1")
        self.assertIsNone(owner["parent_span_id"])
        # Later calls get fresh ids and hang under the span's owner -> a real depth-2 tree.
        self.assertNotEqual(second["span_id"], "S1")
        self.assertEqual(second["parent_span_id"], "S1")
        self.assertEqual(third["parent_span_id"], "S1")
        # One shared trace across the span.
        self.assertEqual(len({c["trace_id"] for c in lt.calls}), 1)

    def test_nested_spans_chain_outer_parent(self):
        lt = FakeClient()
        with trace("T"):
            with span("S1"):
                rec(lt)  # owns S1, parent None
                with span("S2"):
                    rec(lt)  # owns S2, parent S1
                    rec(lt)  # child of S2
                rec(lt)  # child of S1
        s1_owner, s2_owner, s2_child, s1_child = lt.calls
        self.assertEqual(s1_owner["span_id"], "S1")
        self.assertIsNone(s1_owner["parent_span_id"])
        self.assertEqual(s2_owner["span_id"], "S2")
        self.assertEqual(s2_owner["parent_span_id"], "S1")  # inner span chains to the outer
        self.assertEqual(s2_child["parent_span_id"], "S2")
        self.assertEqual(s1_child["parent_span_id"], "S1")
        self.assertTrue(all(c["trace_id"] == "T" for c in lt.calls))

    def test_context_restored_after_exit(self):
        with trace("T"):
            with span("S1"):
                self.assertEqual(current_span_id(), "S1")
            self.assertIsNone(current_span_id())
            self.assertEqual(current_trace_id(), "T")
        self.assertIsNone(current_trace_id())


if __name__ == "__main__":
    unittest.main()
