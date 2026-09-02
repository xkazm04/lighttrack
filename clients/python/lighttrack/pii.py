"""GENERATED FILE - do not edit.

The PII rule set the LightTrack server scrubs ingest with, exported by `crates/anon` to
`clients/contract/fixtures/pii.json` and rendered here so `guard({'no_pii': True})` runs
exactly the rules the ingest path runs. Before this file the SDK carried its own four-row
copy, which had drifted: it still ran the pre-D14 phone regex that flags every ISO date as a
phone number.

Rules are in evaluation order (most specific first) and several may share a `kind`.

Regenerate with `LIGHTTRACK_UPDATE_FIXTURES=1 python -m pytest clients/python/tests` after
changing crates/anon.
"""

#: Family names: email, iban, ssn, secret, phone, credit_card, ip. Patterns are restricted to
#: the RE2 / JS / Python / Rust common subset: no lookaround, no backreferences.
PII_RULES = [
    {"kind": "email", "pattern": "[A-Za-z0-9._%+\\-]+@[A-Za-z0-9.\\-]+\\.[A-Za-z]{2,}", "placeholder": "<EMAIL>"},
    {"kind": "iban", "pattern": "\\b[A-Z]{2}\\d{2}[A-Z0-9]{10,30}\\b", "placeholder": "<IBAN>"},
    {"kind": "ssn", "pattern": "\\b\\d{3}-\\d{2}-\\d{4}\\b", "placeholder": "<SSN>"},
    {"kind": "secret", "pattern": "\\bsk-[A-Za-z0-9_\\-]{16,}\\b", "placeholder": "<SECRET>"},
    {"kind": "secret", "pattern": "\\bAKIA[0-9A-Z]{12,}\\b", "placeholder": "<SECRET>"},
    {"kind": "secret", "pattern": "\\b[0-9a-fA-F]{32,}\\b", "placeholder": "<SECRET>"},
    {"kind": "phone", "pattern": "\\+\\d{1,3}(?:[ \\-](?:\\(\\d{1,4}\\)|\\d{2,4})){2,5}\\b", "placeholder": "<PHONE>"},
    {"kind": "phone", "pattern": "\\+\\d{1,3}[ \\-]?\\d{7,14}\\b", "placeholder": "<PHONE>"},
    {"kind": "credit_card", "pattern": "\\b\\d(?:[ \\-]?\\d){12,18}\\b", "placeholder": "<CC>"},
    {"kind": "ip", "pattern": "\\b(?:25[0-5]|2[0-4]\\d|[01]?\\d\\d?)(?:\\.(?:25[0-5]|2[0-4]\\d|[01]?\\d\\d?)){3}\\b", "placeholder": "<IP>"},
    {"kind": "phone", "pattern": "\\(\\d{2,4}\\)[ \\-]?\\d{2,4}(?:[ \\-]?\\d{2,4}){1,3}\\b", "placeholder": "<PHONE>"},
    {"kind": "phone", "pattern": "\\b\\d{3}[ \\-.]\\d{3}[ \\-.]\\d{4}\\b", "placeholder": "<PHONE>"},
]
