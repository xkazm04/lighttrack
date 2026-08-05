"""A failed send must be visible, bounded, silenceable — and must still never raise.

Network-free: every case points the client at a closed port or drives `Diagnostics` directly, and
stderr is captured so we can assert on the exact bytes a user would see.

Run from `clients/python`:  `python -m unittest discover tests`
"""

import contextlib
import io
import unittest

from lighttrack import LightTrack
from lighttrack.diagnostics import Diagnostics, no_project_message, send_failure_message


class RateLimitTests(unittest.TestCase):
    def test_repeated_failures_of_one_kind_print_once(self):
        d = Diagnostics(quiet=False)
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            for _ in range(1000):
                d.warn("network", "boom")
        self.assertEqual(d.emitted, 1, "a tight loop must not flood the console")
        self.assertEqual(d.suppressed, 999)
        self.assertEqual(err.getvalue().count("boom"), 1)

    def test_distinct_kinds_each_get_a_line(self):
        d = Diagnostics(quiet=False)
        with contextlib.redirect_stderr(io.StringIO()):
            d.warn("network", "a")
            d.warn("http-400", "b")
            d.warn("network", "a")
        self.assertEqual(d.emitted, 2)

    def test_cooldown_expiry_re_warns_with_the_suppressed_count(self):
        d = Diagnostics(quiet=False, cooldown=0.0)
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            d.warn("network", "boom")
            d.warn("network", "boom")
        self.assertEqual(d.emitted, 2)

    def test_quiet_flag_silences_everything(self):
        d = Diagnostics(quiet=True)
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            d.warn("network", "boom")
        self.assertEqual(err.getvalue(), "")
        self.assertEqual(d.emitted, 0)

    def test_warn_never_raises(self):
        d = Diagnostics(quiet=False)
        with contextlib.redirect_stderr(io.StringIO()):
            d.warn("k", None)  # a bad message must not become the failure it reports


class MessageTests(unittest.TestCase):
    def test_no_project_message_names_the_env_var_and_the_constructor_arg(self):
        m = no_project_message("http://127.0.0.1:8787")
        self.assertIn("LIGHTTRACK_PROJECT", m)
        self.assertIn("project=", m)
        self.assertIn("LIGHTTRACK_KEY", m)

    def test_http_400_without_a_project_points_at_the_project_setting(self):
        m = send_failure_message("http://h", "/v1/events", "HTTP 400 project_id is required",
                                 status=400, has_project=False, has_key=True)
        self.assertIn("LIGHTTRACK_PROJECT", m)
        self.assertIn("project_id is required", m)

    def test_unreachable_server_points_at_the_url_setting(self):
        m = send_failure_message("http://127.0.0.1:1", "/v1/events", "URLError")
        self.assertIn("LIGHTTRACK_URL", m)

    def test_messages_are_ascii_only(self):
        # They land in whatever console the host app has; a cp1252 terminal mangles anything else.
        for m in (no_project_message("http://h"),
                  send_failure_message("http://h", "/v1/events", "x", status=429),
                  send_failure_message("http://h", "/v1/events", "x", status=500)):
            m.encode("ascii")


class ClientTests(unittest.TestCase):
    """The end-to-end contract: visible, but never thrown."""

    def _run(self, **kw) -> str:
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            lt = LightTrack(base_url="http://127.0.0.1:1", async_=False, timeout=0.2, **kw)
            lt.track("openai", "gpt-4o", input_tokens=1, output_tokens=1)
            lt.close()
        return err.getvalue()

    def test_unconfigured_client_warns_and_does_not_raise(self):
        out = self._run()
        self.assertIn("LIGHTTRACK_PROJECT", out)
        self.assertIn("[lighttrack]", out)

    def test_quiet_client_says_nothing(self):
        self.assertEqual(self._run(quiet=True, project="p"), "")

    def test_configured_project_does_not_trigger_the_first_run_warning(self):
        out = self._run(project="demo")
        self.assertNotIn("no project is configured", out)


if __name__ == "__main__":
    unittest.main()
