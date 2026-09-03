"""The relay admission verdict as an SDK caller sees it (M18).

Relay calls are functional -- they raise -- but "raised" was all a caller could learn, so an
unroutable action type (nothing in the fleet advertises it, and nothing will until somebody changes
the fleet) was indistinguishable from a timeout worth retrying. `RelayError.code` and
`is_unroutable` are what make that a decision the app can take.

Mirrors `clients/typescript/src/relay.test.ts` case for case.
"""

import json

from lighttrack import RelayError
from lighttrack.client import error_code


def test_an_unroutable_refusal_is_distinguishable_from_a_retryable_failure():
    body = json.dumps(
        {
            "error": {
                "code": "relay_unroutable",
                "message": "no enrolled device advertises 'xprice/typo' (2 device(s) enrolled)",
            }
        }
    )
    refused = RelayError(
        f"POST /v1/relay/tasks -> HTTP 422: {body}",
        code=error_code(body),
        status=422,
    )
    assert refused.is_unroutable
    assert refused.code == "relay_unroutable"
    assert refused.status == 422
    # The reason survives into the message: it names the fix (the spelling, or a device's
    # capabilities), and an SDK that swallowed it would leave the caller with only "422".
    assert "xprice/typo" in str(refused)

    # A transient failure is NOT this. Retrying it is right, and `is_unroutable` is what stops an
    # app burning its retry budget on a task that can never run.
    transient = RelayError("POST /v1/relay/tasks failed: connection refused")
    assert not transient.is_unroutable
    assert transient.code is None
    assert transient.status is None

    overloaded = RelayError("HTTP 503", code="overloaded", status=503)
    assert not overloaded.is_unroutable


def test_a_malformed_error_body_degrades_to_no_code_rather_than_replacing_the_failure():
    # An error response is exactly when a body is least likely to be well-formed -- a proxy's HTML,
    # a truncated stream. Parsing must never turn "the server rejected you" into "invalid JSON".
    assert error_code("<html>502 Bad Gateway</html>") is None
    assert error_code("") is None
    assert error_code("{}") is None
    assert error_code(json.dumps({"error": "a string, not an object"})) is None
    assert error_code(json.dumps({"error": {"code": 422}})) is None
    assert error_code(json.dumps({"error": {"code": "not_found"}})) == "not_found"


def test_relay_error_is_still_a_plain_exception_for_callers_that_only_catch_it():
    # The added fields must not change how the type is caught: existing `except RelayError` code
    # predates them and has to keep working unchanged.
    assert issubclass(RelayError, Exception)
    try:
        raise RelayError("boom")
    except RelayError as e:
        assert str(e) == "boom"
