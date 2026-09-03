"""The enforcement half of pre-spend admission: what the client does with a refusal.

The verdicts themselves are cross-language and live in the contract suite. What is language-local —
and what this file pins — is that `enforce` actually stops the call, that it is off unless asked for,
and that a blocked call is never recorded as spend.
"""

import unittest

from lighttrack import BLOCKED_TAG, BudgetExceeded, LightTrack, parse_limit_view


def at_cap(**kw):
    """A client whose cached view says the project is at its cap."""
    lt = LightTrack(enabled=True, async_=False, quiet=True, journal=False, **kw)
    lt.limits.observe(parse_limit_view(200, {}, {"usage_ratio": 1.0}))
    return lt


class Enforcement(unittest.TestCase):
    def test_admission_is_off_unless_the_app_asks_for_it(self):
        # Adding an observability SDK must not change what an app does. The default has to be inert
        # even when the cached view says the project is over budget.
        lt = at_cap()
        self.assertFalse(lt.admit().ok, "the verdict is still available to read")
        lt.gate("summarize")  # but nothing is enforced

    def test_block_refuses_with_a_typed_error_carrying_the_reason(self):
        lt = at_cap(enforce="block")
        with self.assertRaises(BudgetExceeded) as cm:
            lt.gate("summarize")
        # Typed, because the host app has to tell "your budget said no" (degrade: smaller model,
        # cache, queue) from a provider outage (retry).
        self.assertEqual(cm.exception.reason, "at_cap")
        self.assertIsNone(cm.exception.retry_after_secs)

    def test_warn_reports_and_proceeds(self):
        at_cap(enforce="warn").gate("summarize")

    def test_a_blocked_call_is_traffic_never_spend(self):
        sent = []
        lt = at_cap(enforce="block", record_blocked=True, project="demo")
        lt._post = lambda path, body: sent.append(body)  # type: ignore[assignment]
        with self.assertRaises(BudgetExceeded):
            lt.gate("summarize")
        self.assertEqual(len(sent), 1, "one event for the blocked call")
        ev = sent[0]
        self.assertEqual(ev["tags"], [BLOCKED_TAG])
        # Zero usage and no cost: the call was never made, so inventing spend for it would corrupt
        # exactly the cost report the cap exists to protect.
        self.assertEqual(ev["usage"]["input"], 0)
        self.assertEqual(ev["usage"]["output"], 0)
        self.assertNotIn("cost_usd", ev)
        self.assertEqual(ev["status"], "error")
        self.assertEqual(ev["metadata"]["lt_admit_reason"], "at_cap")

    def test_an_unobserved_client_always_admits(self):
        # Fail open: "unknown" must never read as "over budget", or installing LightTrack is an
        # outage.
        lt = LightTrack(async_=False, quiet=True, journal=False, enforce="block")
        self.assertTrue(lt.admit().ok)
        lt.gate("summarize")


if __name__ == "__main__":
    unittest.main()
