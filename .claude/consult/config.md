# Consult skill — project overlay

No `.ai/manifest.yaml` field currently names a project-specific consult log location, so this file
is that location by the skill's own fallback rule (`.claude/consult/config.md` for Claude).

## Skill improvement log

### 2026-09-16 — gateway response-caching decision (lighttrack)

Asked whether `lt-gateway` (`crates/gateway`) should cache identical `/v1/chat/completions`
answers, and how to key/bound/invalidate them without serving stale answers across seats.

Two observations worth keeping for the next consult that touches this gateway or a similar one:

- **The registry has no subject that owns "cache a full LLM response for a repeated identical
  request."** `deterministic-prefix-caching`'s own Boundaries section explicitly routes "a prompt
  sent to a language model" to `llm-agent/prompt-and-context/prompt-assembly` instead of claiming
  it — and `prompt-assembly` turns out to own provider-side *prompt-prefix* caching (session
  fingerprinting, cache breakpoints for billing), not *response* memoization for repeated
  completions. The applicable material was the domain-general caching subjects
  (`client-architecture/client-fetch-cache`, `backend-platform/data-layer/read-serving-replicas`)
  plus one technique borrowed from `backend-platform/inference-serving/paged-block-cache`
  (`salt-as-a-cache-partition`, for the cross-tenant/cross-seat partition question), applied by
  analogy to a backend HTTP gateway rather than a browser client or a DB replica. Worth a
  `llm-observability` or `software-engineering/llm-agent` subject on "LLM completion response
  caching" if this pattern recurs across projects — logged here as an observation, not filed as a
  registry change (this checkout is a pinned/ordinary copy, not a dev link).
- **This repo's own engine already stamps a `Determinism` fact per provider path**
  (`crates/engine/src/lib.rs`): `openai`/`gemini` get `Exact` (pinned seed), everything the
  gateway actually fronts by default — `claude -p`, `codex exec`, the plain Anthropic Messages API,
  a generic `http_target` — gets `BestEffort`, and self-consistency draws get `Sampled`. That
  single fact should gate any future caching design here: `admission-hypothesis`'s "state the bet"
  discipline cannot assume identical-request-implies-identical-answer on the CLI seats this
  gateway exists for, so a response cache is a bounded bet on retry/double-submit convergence, not
  a correctness-preserving memoization the way a build cache or a DB read is.
