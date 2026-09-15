---
note_type: engine-call-site
call_site: gateway-chat-completion
task: Serve an OpenAI-compatible conversation through a configured provider/model failover chain
use_case: UC3
modality: text
entry: crates/gateway/src/chat.rs:73
wrapper: gateway ChainRun -> lighttrack_engine::providers::generate_chat
prompt_builder: crates/gateway/src/wire.rs:100
output_contract: optional caller schema at crates/gateway/src/wire.rs:138-158 -> response at crates/gateway/src/chat.rs:136-186
providers: [anthropic, google, openai, openrouter, codex]
model: configured route or literal provider/model@effort
grounding: "4/4 in-direction ; out-direction closed for response provenance, prompt fingerprint absent"
dials: { wrapping: 8/10, observability: 8/10, caching: 2/10 }
fingerprint: ac1e1c22af6824edd520af55f95bd0814d59f2f8fe981d72e2e272db3a7a1089
status: discovered
characters: ["[[maya-chen]]", "[[tomas-novak]]", "[[priya-raman]]", "[[elena-garcia]]"]
last_reviewed: 2026-09-15 (init; no session)
tags: [engine, llm-call-site]
---
# Engine call site - gateway-chat-completion

## What the model is asked to do

Continue a caller-supplied conversation, optionally under system/developer instructions, tools, and
a response schema. The gateway resolves a named route or literal target, rejects incompatible tool
chains before spend, checks admission, runs bounded failover, and returns an OpenAI-shaped response.

## Grounding audit (Lens B) - grounding 4/4

1. System/developer instructions - collected in order at crates/gateway/src/wire.rs:105-137.
2. Every supported user/assistant/tool turn - validated and preserved at :107-132.
3. Response schema or JSON-object request - mapped at :138-158.
4. Tool definitions, tool choice, prior calls, and results - mapped at :119-127,159-170.

Prompt evidence is caller-owned and dynamic. Multi-turn CLI targets receive a rendered transcript at
crates/engine/src/chat.rs:92-119; OpenAI-shaped providers receive native messages. No request/output
sample was captured at init.

Out direction is strong: route, served target, provider-reported model, fallback/attempt chain,
cost, tokens, reasoning tokens, determinism, schema state, transcript folding, request id, and tool
calls reach the response. Telemetry content capture/redaction and response parity require a run.

## Lens A - Engine Quality dials

- Wrapping 8/10: common route/failover/provider seams, pre-spend admission, tool capability check,
  per-attempt timeout/retry, provider schema state, and typed wire mapping. The request accepts and
  ignores SDK temperature/max_tokens at crates/gateway/src/wire.rs:28-31; field/count/length bounds
  and disconnect cancellation need verification.
- Observability 8/10: request id and rich provenance are immediate and telemetry is emitted.
  Generic prompt fingerprinting and guaranteed synchronous telemetry durability are not evident.
- Caching 2/10: HTTP connection reuse and seat cooldowns exist; no response cache, provider prefix
  cache marker, or identical in-flight request dedup was discovered.

## Lens C - model fit

This surface is workload-dependent and mixes bounded schema/tool calls with open conversation.
Benchmark per declared use case, not one generic chat fixture. Keep the request fixed and compare
route cells; a premium wins only when bound Characters see a material result, not because it emits
more prose.

## Findings

None. Init inventories; tiger run is required to open or certify findings.

## Strengths (do not refactor away)

Tool incompatibility fails before spend; every failover attempt remains visible; schema shedding and
determinism are response fields; native tool traffic is never silently flattened onto an incapable provider.

Fingerprint inputs: crates/gateway/src/wire.rs:14-31 and :67-174 plus
crates/engine/src/chat.rs:56-193.
