---
note_type: engine-call-site
call_site: benchmark-http-target
task: Ask an operator-owned HTTP application for a benchmark candidate
use_case: UC2
modality: text
entry: crates/engine/src/http_target.rs:125
wrapper: lighttrack_engine::http_target::generate_http
prompt_builder: crates/engine/src/http_target.rs:37
output_contract: crates/engine/src/http_target.rs:49-70 -> crates/engine/src/http_target.rs:132-139
providers: [operator-http-endpoint]
model: opaque endpoint configuration; URL is the reported identity
grounding: "2-3/3 in-direction ; out-direction closed for returned fields, internal model provenance remains opaque"
dials: { wrapping: 8/10, observability: 7/10, caching: 1/10 }
fingerprint: 2080ed857c86acd022b65a39591a456e2d64c4f42e2d491acf5bca0eb23f0d57
status: discovered
characters: ["[[maya-chen]]", "[[elena-garcia]]", "[[daniel-brooks]]"]
last_reviewed: 2026-09-15 (init; no session)
tags: [engine, llm-call-site]
---
# Engine call site - benchmark-http-target

## What the model is asked to do

LightTrack does not call a model directly here. It posts a benchmark case to an operator-owned RAG
pipeline, agent, or application and treats the returned output as the candidate. The endpoint may
report its aggregate usage, latency, and cost.

## Grounding audit (Lens B) - grounding 2-3/3

1. Case input - always serialized at crates/engine/src/http_target.rs:103-107.
2. Resolved system prompt - included when the target has one.
3. Reference/expected answer - included when the dataset has one.

The HTTP request schema is the prompt contract. Whether the external app actually uses optional
fields is outside LightTrack and must be evidenced by endpoint/version-specific fixtures. No
endpoint was called at init.

Out direction requires non-empty output; usage, endpoint latency, and direct cost are optional and
remain absent when unknown. Local wall clock fills only latency. Internal retrieval/model/prompt
provenance is not inferred.

## Lens A - Engine Quality dials

- Wrapping 8/10: shared bounded HTTP client/read, typed request/response, transient retry,
  optional HMAC over exact body, and empty-output rejection. The endpoint exposes no sampling,
  cancellation, or schema knob, so determinism is honestly BestEffort.
- Observability 7/10: URL identity, output, optional usage/cost/latency, and local wall clock survive;
  internal attempts/models are opaque by contract.
- Caching 1/10: no semantic result/prefix/in-flight cache was discovered. Retrying can buy endpoint
  work again; idempotency is an endpoint concern.

## Lens C - model fit

LightTrack cannot vary the endpoint's hidden model or thinking. Treat each versioned endpoint
configuration as a distinct target and require its own provenance. Compare complete applications,
not invented model cells.

## Findings

None. Init inventories; tiger run is required to open or certify findings.

## Strengths (do not refactor away)

Opaque systems remain honestly Other/BestEffort/unpriced; optional HMAC authenticates exact bytes;
unknown usage is not zero; empty output is a failure.

Fingerprint inputs: crates/engine/src/http_target.rs:37-70 and :91-155.
