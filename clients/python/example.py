"""Runnable example for the LightTrack Python client.

Start the API first (defaults to http://127.0.0.1:8787), then:

    python example.py                                   # dev mode: ingests into project "demo"
    LIGHTTRACK_KEY=lt_... python example.py             # enforced mode: project from the key

Uses fake provider response objects, so it runs with no real provider SDK or API key.
"""

import os
import types

from lighttrack import LightTrack, guard, span, trace


def fake_openai_response():
    usage = types.SimpleNamespace(
        prompt_tokens=120, completion_tokens=45,
        prompt_tokens_details=types.SimpleNamespace(cached_tokens=64),
    )
    return types.SimpleNamespace(model="gpt-4o-mini", usage=usage)


def fake_openai_client():
    """A stand-in for `OpenAI()` — same shape, so `lt.wrap(...)` instruments it identically."""
    completions = types.SimpleNamespace(create=lambda **kw: fake_openai_response())
    return types.SimpleNamespace(chat=types.SimpleNamespace(completions=completions))


def fake_anthropic_response():
    usage = types.SimpleNamespace(input_tokens=200, output_tokens=80, cache_read_input_tokens=0)
    return types.SimpleNamespace(model="claude-haiku-4-5", usage=usage)


def fake_gemini_response():
    return {"model_version": "gemini-2.5-flash",
            "usage_metadata": {"prompt_token_count": 90, "candidates_token_count": 30}}


def main():
    # `project` is used in dev mode / by admin keys; a project key would override it server-side.
    project = None if os.environ.get("LIGHTTRACK_KEY") else os.environ.get("LIGHTTRACK_PROJECT", "demo")
    with LightTrack(project=project, source="example.py", tags=["demo"]) as lt:
        lt.track_openai(fake_openai_response(), latency_ms=210, trace_id="t-1")
        lt.track_anthropic(fake_anthropic_response(), latency_ms=540)
        lt.track_gemini(fake_gemini_response(), latency_ms=300)

        # Timing span: latency measured automatically; usage pulled from the response.
        with lt.span("openai", None, tags=["span"]) as s:
            s.set_openai(fake_openai_response())

        # Low-level generic call.
        lt.track("openai", "gpt-4o", input_tokens=10, output_tokens=5, operation="chat")

        # One-line auto-instrumentation: wrap the SDK client once, then every call is captured with
        # no per-call code. `trace()` shares one trace_id; `span()` nests calls as child spans.
        # (`import lighttrack.auto` patches the real OpenAI/Anthropic/Gemini SDKs globally instead.)
        client = lt.wrap(fake_openai_client())
        with trace():
            client.chat.completions.create(model="gpt-4o", messages=[])  # auto-tracked (trace root)
            with span():
                client.chat.completions.create(model="gpt-4o", messages=[])  # auto-tracked (child)

        # Inline output guardrails: `guard` is pure (returns a verdict); `track_guard` also records
        # the verdict as a score so guardrail pass-rates are observable.
        print("guard:", guard('{"a":1}', {"json_keys": ["a", "b"]}).violations)  # -> missing 'b'
        verdict = lt.track_guard('{"merchant":"Acme","total":12.5}',
                                 {"json_keys": ["merchant", "total"], "no_pii": True}, name="extract")
        print("track_guard ok:", verdict.ok)
        lt.flush()
    print("sent 7 events + 1 guard score — check: GET /v1/events, /v1/scores, /v1/costs")


if __name__ == "__main__":
    main()
