# OTLP ingest — OpenTelemetry GenAI spans

LightTrack accepts **OTLP/HTTP JSON** trace exports on `POST /v1/traces` and maps spans that follow
the [OpenTelemetry GenAI semantic conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/)
onto its native `LlmEvent`. If your app is already instrumented with OpenTelemetry, you do not need
the LightTrack SDK: point the exporter at the API and the traces land as events, complete with cost,
limits, redaction, and the trace tree.

`/v1/traces` is the standard OTLP/HTTP path, so an exporter configured with
`OTEL_EXPORTER_OTLP_ENDPOINT=http://<lighttrack-host>:8787` works with no path override.

**In scope:** HTTP + JSON, traces only. **Not** in scope: gRPC OTLP, the metrics/logs signals,
protobuf bodies, and a collector/proxy mode.

## Auth & project scoping

Identical to `POST /v1/events` — same guard, same resolution order:

- `Authorization: Bearer <project key>` → spans are forced into that key's project (any project
  attribute or query param in the request is ignored). Set it with `OTEL_EXPORTER_OTLP_HEADERS`.
- An admin key (or dev mode) must say which project: either the `lighttrack.project_id` attribute
  (resource-level or span-level) or the `?project=<id>` query param. A span with neither comes back
  `invalid` / `bad_request`, per span.

## Attribute mapping

The conventions have churned and three widely-deployed instrumentations extend them, so every field
reads a list of accepted names, **newest first**. All of these are accepted today:

| `LlmEvent` field | attributes, in precedence order |
|---|---|
| `provider` | `gen_ai.provider.name`, `gen_ai.system`, `llm.provider`, `llm.system`, `ai.model.provider` |
| `model` | `gen_ai.request.model`, `gen_ai.response.model`, `llm.model_name`, `llm.request.model`, `ai.model.id` |
| `operation` | `gen_ai.operation.name`, `llm.operation.name`, `openinference.span.kind` |
| `usage.input` | `gen_ai.usage.input_tokens`, `gen_ai.usage.prompt_tokens` *(legacy)*, `llm.token_count.prompt`, `llm.usage.prompt_tokens`, `ai.usage.promptTokens` |
| `usage.output` | `gen_ai.usage.output_tokens`, `gen_ai.usage.completion_tokens` *(legacy)*, `llm.token_count.completion`, `llm.usage.completion_tokens`, `ai.usage.completionTokens` |
| `usage.cached_input` | `gen_ai.usage.cached_input_tokens`, `gen_ai.usage.cache_read_input_tokens`, `llm.token_count.prompt_details.cache_read` |
| `usage.reasoning` | `gen_ai.usage.reasoning_tokens`, `gen_ai.usage.output_reasoning_tokens`, `llm.token_count.completion_details.reasoning` |
| `cost_usd` | `gen_ai.usage.cost`, `gen_ai.usage.total_cost`, `llm.usage.total_cost` (non-standard; when absent the call is priced from the DB price book) |
| `input` | `gen_ai.input.messages`, `gen_ai.prompt`, `llm.prompts`, `input.value` |
| `output` | `gen_ai.output.messages`, `gen_ai.completion`, `llm.completions`, `output.value` |
| `name` | `lighttrack.name`, else the span name |
| `project_id` | `lighttrack.project_id`, else `?project=` |

Structural fields:

- `trace_id` / `span_id` / `parent_span_id` come straight from the span (hex, lowercased), so
  `GET /v1/traces/:id` renders OTel-sourced traces with no extra work.
- `ts` = `startTimeUnixNano`; `latency_ms` = `end − start`.
- `status` = `success` unless the span status is `STATUS_CODE_ERROR`, in which case `error` — or
  `timeout` when `error.type` / the status message reads as one. The failure detail comes from the
  status message, else a recorded `exception` event's `exception.message`, else `error.type`.
- `source` = `"otlp"`, and `metadata.otel` keeps the raw `gen_ai.system` string, the *other* model
  attribute, `gen_ai.response.id`, finish reasons, the instrumentation scope and `service.name`.
- The event id is `"<traceId>-<spanId>"` — deterministic, so an exporter retry replays into the
  existing duplicate-acknowledgement path instead of double-counting.

Notes and deliberate gaps:

- `model` prefers the **request** model over the response model: response models carry a dated
  suffix (`claude-haiku-4-5-20260101`) that misses the price book and fragments rollups. The response
  model is preserved at `metadata.otel.response_model`.
- `gen_ai.usage.total_tokens` **alone** is not mapped — a total can't be split into input/output
  without inventing cost.
- IDs must be hex, as the OTLP/JSON spec requires. A non-conforming encoder that base64s them is not
  decoded; the ids pass through verbatim (see *One id, both doors* for the case rule).
- An unmodeled `gen_ai.system` (bedrock, mistral, groq, …) is accepted as provider `unknown` and
  stays unpriced — visible rather than silently dropped.

## Trace shape

A GenAI span's parent is usually **not** a GenAI span: the LLM call hangs off an HTTP handler, a tool
span or a DB span carrying no `gen_ai.*` attributes. Those spans are refused (`not_genai`) and never
stored — the event table is for LLM calls, and inventing phantom events for them would corrupt cost,
token and span accounting.

Their *linkage* is kept, though. Each mapped span is reparented onto the nearest ancestor in the
export that also mapped, so the trace keeps the shape it had in the exporter instead of fragmenting
into one root per dropped parent. The exporter's original parent is preserved at
`metadata.otel.otlp_parent_span_id`, so a synthesized link is visible as synthesized. Only the spans
in the same export are visible: a parent exported in a different batch leaves its child a root, as
before.

## One id, both doors

Trace/span ids are canonicalized identically on the OTLP door and the native `POST /v1/events` door
(`core::normalize_trace_ref`). A W3C id — 16 or 32 hex characters — is **case-insensitive** by spec
and is lower-cased; anything else is a caller's own opaque id (`"req-1"`, `"Order-7"`) and is kept
verbatim, because folding those would merge distinct traces. That is what lets a single end-to-end
trace spanning an OTel-instrumented service and an SDK-instrumented service render as one trace: the
OTLP door already lower-cased, the SDK door normalized nothing, and the two halves silently split.

*Migration:* events stored before this change keep whatever case they arrived with. An old trace is
intact and still readable under its own id, but if an SDK was sending upper-case W3C ids, new spans
land under the lower-cased id — the old and new halves of that one trace stay separate. Nothing is
rewritten in place.

## Guarantees

Mapping is the *only* thing the OTLP handler does. The mapped events are handed to the same handler
that serves `POST /v1/events/batch`, so validation, the project's payload-persistence policy, PII
redaction, price-book costing and single-critical-section limit admission are byte-for-byte the
native behavior. **OTLP is not a side door around a cap or a redaction policy.** Prompts and
completions land in the redactable `input`/`output` fields, never in `metadata`.

## Response

HTTP **200** with the OTLP `ExportTraceServiceResponse` shape, so a stock exporter parses it:

```json
{
  "partialSuccess": { "rejectedSpans": 1, "errorMessage": "1 of 3 span(s) not recorded …" },
  "lighttrack": {
    "accepted": 2, "unmapped": 1, "rejected": 0, "invalid": 0,
    "results": [
      { "index": 0, "spanId": "eee19b7ec3c1b174", "status": "accepted",
        "id": "5b8efff798038103d269b633813fc60c-eee19b7ec3c1b174" },
      { "index": 2, "spanId": "eee19b7ec3c1b176", "status": "unmapped", "code": "not_genai",
        "reason": "span carries no GenAI attributes (…)" }
    ]
  }
}
```

`partialSuccess` is **omitted** on a clean export (the spec's success shape) and `rejectedSpans`
counts every span that was not stored. The additive `lighttrack` object — which OTLP consumers ignore
as an unknown field — carries the per-span detail in the batch endpoint's code taxonomy, so one
client branch covers both front doors. Nothing is ever silently dropped: every span in the request
appears in `results`.

| `code` | meaning |
|---|---|
| `not_genai` | the span carries no GenAI attributes at all — not an LLM call (OTLP-only code) |
| `bad_request` | GenAI-shaped but unmappable or invalid (e.g. no model attribute, no project) |
| `rate_limited` | an enforcing limit breach turned it away; not stored |
| `conflict` | that span id already exists with a different payload |
| `internal` | store failure on that item; sibling spans still committed |

Request-level failures still use the normal error envelope: `401 unauthorized` for a bad key,
`413` over `LIGHTTRACK_MAX_BATCH_BODY_BYTES`, `400 bad_request` over `LIGHTTRACK_MAX_BATCH` spans.

## Try it with curl

```bash
curl -sS http://localhost:8787/v1/traces \
  -H 'content-type: application/json' \
  -H "authorization: Bearer $LIGHTTRACK_API_KEY" \
  -d '{
  "resourceSpans": [{
    "resource": { "attributes": [
      { "key": "service.name", "value": { "stringValue": "checkout-api" } }
    ]},
    "scopeSpans": [{
      "scope": { "name": "opentelemetry.instrumentation.anthropic" },
      "spans": [{
        "traceId": "5b8efff798038103d269b633813fc60c",
        "spanId": "eee19b7ec3c1b174",
        "name": "chat claude-haiku-4-5",
        "kind": 3,
        "startTimeUnixNano": "1785578400000000000",
        "endTimeUnixNano": "1785578401500000000",
        "attributes": [
          { "key": "gen_ai.system",             "value": { "stringValue": "anthropic" } },
          { "key": "gen_ai.operation.name",     "value": { "stringValue": "chat" } },
          { "key": "gen_ai.request.model",      "value": { "stringValue": "claude-haiku-4-5" } },
          { "key": "gen_ai.usage.input_tokens", "value": { "intValue": "1200" } },
          { "key": "gen_ai.usage.output_tokens","value": { "intValue": "340" } }
        ],
        "status": { "code": 1 }
      }]
    }]
  }]
}'
```

Then read it back:

```bash
curl -sS "http://localhost:8787/v1/traces/5b8efff798038103d269b633813fc60c" \
  -H "authorization: Bearer $LIGHTTRACK_API_KEY"
```

## Wiring a real OTel SDK exporter

Environment-only (works for every OTel SDK and the Collector's `otlphttp` exporter — note the
**`http/json`** protocol, since LightTrack does not accept protobuf):

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT="http://localhost:8787"
export OTEL_EXPORTER_OTLP_PROTOCOL="http/json"
export OTEL_EXPORTER_OTLP_HEADERS="authorization=Bearer $LIGHTTRACK_API_KEY"
export OTEL_SERVICE_NAME="checkout-api"
```

Python, wiring it in code with an instrumentation that already emits GenAI spans:

```python
from opentelemetry import trace
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry.instrumentation.anthropic import AnthropicInstrumentor  # or OpenAIInstrumentor

provider = TracerProvider()
provider.add_span_processor(
    BatchSpanProcessor(
        OTLPSpanExporter(
            endpoint="http://localhost:8787/v1/traces",
            headers={"authorization": f"Bearer {LIGHTTRACK_API_KEY}"},
        )
    )
)
trace.set_tracer_provider(provider)
AnthropicInstrumentor().instrument()
# Every Anthropic call is now a GenAI span → a LightTrack event.
```

> The stock `opentelemetry-exporter-otlp-proto-http` package sends **protobuf**. Use the JSON
> exporter (`OTEL_EXPORTER_OTLP_PROTOCOL=http/json`, or a Collector `otlphttp` exporter with
> `encoding: json`) until protobuf support lands. A Collector hop is the zero-code path for fleets
> that are already exporting protobuf:

```yaml
# collector.yaml — fan OTel traces out to LightTrack as OTLP/JSON
exporters:
  otlphttp/lighttrack:
    endpoint: http://localhost:8787
    encoding: json
    headers:
      authorization: "Bearer ${LIGHTTRACK_API_KEY}"
service:
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [otlphttp/lighttrack]
```
