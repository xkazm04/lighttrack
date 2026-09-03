"""The cross-language SDK contract, run against the Python client.

Every case here also runs, unchanged, in `clients/typescript/src/contract.test.ts` and
`clients/rust/tests/contract.rs`. That is the whole point: the three SDKs were three
hand-synchronised implementations of one contract, and nothing could see the drift between them —
the provider extractors were triplicated, the PII table was triplicated and one of the three had
gone stale against the server, and CI ran the suites as unrelated jobs. Shared vectors turn "we
believe these agree" into a test.

A behaviour that is not in `clients/contract/fixtures/` is not part of the contract, and a behaviour
that is may not differ between languages. Capabilities a given SDK does not have are declared
`not_supported` in its `lighttrack.manifest.json` and skipped here, honestly and visibly, rather than
quietly not asserted.

Written as `unittest` so it runs under both `python -m pytest clients/python/tests` and the
`python -m unittest discover` form CI uses against the installed wheel.
"""

import json
import os
import re
import unittest
from pathlib import Path

from lighttrack import AdmissionCache, guard, parse_limit_view, shed_ticket, unsettled
from lighttrack.client import _extract_anthropic, _extract_gemini, _extract_openai
from lighttrack.diagnostics import diagnostic_kind, send_failure_message
from lighttrack.pii import PII_RULES

# tests/ -> clients/python -> clients
_CLIENTS = Path(__file__).resolve().parents[2]
_FIXTURES = _CLIENTS / "contract" / "fixtures"
_MANIFEST = _CLIENTS / "python" / "lighttrack.manifest.json"
_PII_MODULE = _CLIENTS / "python" / "lighttrack" / "pii.py"


def fixture(name):
    return json.loads((_FIXTURES / f"{name}.json").read_text(encoding="utf-8"))


MANIFEST = json.loads(_MANIFEST.read_text(encoding="utf-8"))


def supports(capability):
    return MANIFEST.get("capabilities", {}).get(capability) == "supported"


def render_pii_module(rules):
    """The generated `lighttrack/pii.py`.

    The fixture is exported from `crates/anon` (the server's own scrubber). The wheel ships only the
    `lighttrack` package, so it cannot read a JSON file two directories up at import time — the table
    is emitted as a module and this test is what keeps it honest.
    Regenerate: `LIGHTTRACK_UPDATE_FIXTURES=1 python -m pytest clients/python/tests`.
    """
    rows = "\n".join(
        '    {{"kind": {}, "pattern": {}, "placeholder": {}}},'.format(
            json.dumps(r["kind"]), json.dumps(r["pattern"]), json.dumps(r["placeholder"])
        )
        for r in rules
    )
    return (
        '"""GENERATED FILE - do not edit.\n'
        "\n"
        "The PII rule set the LightTrack server scrubs ingest with, exported by `crates/anon` to\n"
        "`clients/contract/fixtures/pii.json` and rendered here so `guard({'no_pii': True})` runs\n"
        "exactly the rules the ingest path runs. Before this file the SDK carried its own four-row\n"
        "copy, which had drifted: it still ran the pre-D14 phone regex that flags every ISO date as a\n"
        "phone number.\n"
        "\n"
        "Rules are in evaluation order (most specific first) and several may share a `kind`.\n"
        "\n"
        "Regenerate with `LIGHTTRACK_UPDATE_FIXTURES=1 python -m pytest clients/python/tests` after\n"
        "changing crates/anon.\n"
        '"""\n'
        "\n"
        "#: Family names: email, iban, ssn, secret, phone, credit_card, ip. Patterns are restricted to\n"
        "#: the RE2 / JS / Python / Rust common subset: no lookaround, no backreferences.\n"
        "PII_RULES = [\n" + rows + "\n]\n"
    )


class PiiTableIsTheServers(unittest.TestCase):
    def test_generated_module_matches_the_fixture(self):
        rules = fixture("pii")["rules"]
        rendered = render_pii_module(rules)
        if os.environ.get("LIGHTTRACK_UPDATE_FIXTURES"):
            _PII_MODULE.write_text(rendered, encoding="utf-8", newline="")
            return
        self.assertEqual(
            _PII_MODULE.read_text(encoding="utf-8"),
            rendered,
            "lighttrack/pii.py has drifted from clients/contract/fixtures/pii.json. The server's "
            "scrubber is the source of truth; regenerate with "
            "`LIGHTTRACK_UPDATE_FIXTURES=1 python -m pytest clients/python/tests`.",
        )
        self.assertEqual(list(PII_RULES), rules, "the imported table must equal the fixture")

    def test_every_pattern_compiles_in_python(self):
        for rule in PII_RULES:
            re.compile(rule["pattern"])


class Extractors(unittest.TestCase):
    def test_provider_extractors(self):
        for case in fixture("extractors")["extractors"]:
            with self.subTest(case["name"]):
                fn = {
                    "openai": _extract_openai,
                    "anthropic": _extract_anthropic,
                    "gemini": _extract_gemini,
                }[case["provider"]]
                model, inp, out, cached = fn(case["response"])
                self.assertEqual(
                    {
                        "model": model,
                        "input_tokens": inp,
                        "output_tokens": out,
                        "cached_input_tokens": cached,
                    },
                    case["expect"],
                    case.get("why", ""),
                )


class Guard(unittest.TestCase):
    def test_guard_verdicts(self):
        for case in fixture("guard")["guard"]:
            with self.subTest(case["name"]):
                result = guard(case["output"], case["rules"])
                failed = sorted(k for k, passed in result.checks.items() if not passed)
                self.assertEqual(failed, sorted(case["expect"]["violations"]), case.get("why", ""))
                self.assertEqual(result.ok, case["expect"]["ok"])
                # `ok` is defined as "nothing failed" — the two must never disagree.
                self.assertEqual(result.ok, len(result.violations) == 0)


@unittest.skipUnless(supports("journal"), "this SDK declares journal=not_supported")
class Journal(unittest.TestCase):
    def test_unsettled_records(self):
        for case in fixture("journal")["journal"]:
            with self.subTest(case["name"]):
                self.assertEqual(unsettled(case["body"]), case["expect"], case.get("why", ""))


class Limits(unittest.TestCase):
    def test_ingest_limit_signals(self):
        for case in fixture("limits")["limits"]:
            with self.subTest(case["name"]):
                v = parse_limit_view(case["status"], case.get("headers"), case.get("body"))
                self.assertEqual(
                    {
                        "accepted": v.accepted,
                        "rate_limited": v.rate_limited,
                        "usage_ratio": v.usage_ratio,
                        "shed_fraction": v.shed_fraction,
                        "retry_after_secs": v.retry_after_secs,
                        "error_code": v.error_code,
                        "binding_scope": (
                            None if v.binding_scope is None
                            else {"kind": v.binding_scope.kind, "value": v.binding_scope.value}
                        ),
                        "binding_rule": v.binding_rule,
                    },
                    case["expect"],
                    case.get("why", ""),
                )


class Admission(unittest.TestCase):
    def test_shed_lottery_is_the_servers_own_function(self):
        for case in fixture("limits")["shed_lottery"]:
            with self.subTest(f'{case["rule_id"]}/{case["event_id"]}'):
                got = shed_ticket(case["rule_id"], case["event_id"])
                self.assertAlmostEqual(got, case["ticket"], delta=1e-12)

    @unittest.skipUnless(supports("admit"), "SDK declares no pre-spend admission")
    def test_pre_spend_admission_verdicts(self):
        for case in fixture("limits")["admission"]:
            with self.subTest(case["name"]):
                cache = AdmissionCache(ttl_ms=case["ttl_ms"])
                for o in case["observe"]:
                    cache.observe(
                        parse_limit_view(o["status"], o.get("headers"), o.get("body")),
                        now_ms=o["at_ms"],
                    )
                v = cache.admit(
                    name=case["admit"].get("name"),
                    event_id=case["admit"].get("event_id"),
                    now_ms=case["admit"]["at_ms"],
                )
                self.assertEqual(
                    {"ok": v.ok, "reason": v.reason,
                     "retry_after_secs": v.retry_after_secs, "stale": v.stale},
                    case["expect"],
                    case.get("why", ""),
                )


class Diagnostics(unittest.TestCase):
    def test_failure_diagnostics(self):
        for case in fixture("diagnostics")["diagnostics"]:
            with self.subTest(case["name"]):
                status = case.get("status")
                self.assertEqual(diagnostic_kind(status), case["kind"])
                msg = send_failure_message(
                    "http://127.0.0.1:8787",
                    "/v1/events",
                    "boom",
                    status=status,
                    has_project=case.get("has_project", False),
                    has_key=case.get("has_key", False),
                )
                for needle in case["hint_contains"]:
                    self.assertIn(needle, msg, case.get("why", ""))
                # ASCII only. These lines land in whatever console the host app has, and a cp1252
                # Windows terminal turns a stray em dash into mojibake.
                self.assertTrue(msg.isascii(), f"message must be ASCII: {msg}")


if __name__ == "__main__":
    unittest.main()
