---
note_type: model-matrix
price_snapshot: 2026-05-31 (config/pricing.json:7,15-28; USD per MTok)
---
# Model x thinking-level matrix

## Price basis (dated repository snapshot)

These are LightTrack's checked-in seed rates, not independently refreshed public prices. Cached
input is included where the app records it.

| Tier | Model | $ in / MTok | $ out / MTok | $ cached in / MTok | Notes |
|---|---|---:|---:|---:|---|
| small | anthropic/claude-haiku-4-5 | 1.00 | 5.00 | 0.10 | Stored benchmark default is haiku |
| standard | anthropic/claude-sonnet-4-6 | 3.00 | 15.00 | 0.30 | Responder and relay-action fallback default is sonnet |
| premium | anthropic/claude-opus-4-8 | 5.00 | 25.00 | 0.50 | Runner CLI default judge spec is opus@xhigh |
| small | openai/gpt-5.4-mini | 0.75 | 4.50 | 0.075 | Candidate and gateway option |
| standard | openai/gpt-5.4 | 2.50 | 15.00 | 0.25 | Candidate and gateway option |
| small | openai/gpt-5.4-nano | 0.20 | 1.25 | 0.02 | Cheap bounded-task challenger |
| standard | google/gemini-3.1-pro | 2.00 | 12.00 | unknown | Candidate and gateway option |

## Benchmark matrix (rows to run later)

| # | Model | Thinking | Pre-registered hypothesis |
|---:|---|---|---|
| 1 | anthropic/claude-haiku-4-5 | low | Cheap floor candidate for tightly anchored scalar/rubric output; likely weak on repo diagnosis |
| 2 | anthropic/claude-haiku-4-5 | medium | May clear bounded judging when schema, anchors, and multi-sample aggregation carry the task |
| 3 | anthropic/claude-sonnet-4-6 | low | Likely floor for anonymization and grounded operational summaries |
| 4 | anthropic/claude-sonnet-4-6 | medium | Likely floor for responder diagnosis and most device actions |
| 5 | anthropic/claude-sonnet-4-6 | high | Challenger for ambiguous diagnosis; may not justify extra latency on short structured verdicts |
| 6 | anthropic/claude-opus-4-8 | high | Premium quality challenger for hostile or ambiguous judge cases |
| 7 | anthropic/claude-opus-4-8 | xhigh | Current runner judge default; ceiling candidate whose incremental value must be measured |
| 8 | openai/gpt-5.4-mini | low | Cross-family low-cost challenger for bounded generation and judging |
| 9 | openai/gpt-5.4-mini | medium | Cross-family floor candidate when low misses nuanced anchors |
| 10 | openai/gpt-5.4 | medium | Cross-family standard challenger for unbounded reasoning |
| 11 | openai/gpt-5.4 | high | Premium diagnosis/judge challenger; cost and latency risk |
| 12 | google/gemini-3.1-pro | low | Independent-family candidate for cross-family judge validation |
| 13 | google/gemini-3.1-pro | high | Ceiling challenger on long-context grounded tasks |

Rows unsupported by a provider/model are recorded as unavailable, never silently substituted.

## Results

No result rows exist. Init does not call models or fabricate quality, cost, or latency deltas.

| Call site | Model | Thinking | Cleared must-pass? | quality_delta | cost_delta | Result |
|---|---|---|---|---|---|---|
| none yet | none | none | unmeasured | unmeasured | unmeasured | requires tiger benchmark |

## Theoretical floor / ceiling hypotheses

| Prompt family | Current selection | Predicted floor to test | Ceiling question |
|---|---|---|---|
| Structured, batched, pairwise, and freeform judges | Runner default opus@xhigh; stored benchmark default haiku | haiku@medium or gpt-5.4-mini@medium, with hostile and borderline fixtures | Does opus materially improve human agreement or repeatability after local clamps and aggregation? |
| Benchmark candidate generation | Per target | Workload-specific small model | Does a larger model move must-pass Character verdicts on the same frozen cases? |
| Dataset anonymization | Runner-selected model | sonnet@low, after deterministic scrub | Does premium reasoning find more residual PII without changing meaning? |
| Healing recommendations | Runner-selected model | sonnet@low | Does extra effort add grounded specificity when the prompt has aggregates but no failure exemplars? |
| Gateway chat | Route configuration | Per use-case, not one global floor | Does the premium target improve the declared caller job enough to justify latency and fallback scarcity? |
| Responder investigations | sonnet | sonnet@medium | Does higher effort improve root-cause evidence and not merely length? |
| Responder auto-fix | sonnet | sonnet@medium, gated | Does a premium row improve correct minimal patches under the same diagnosis and tests? |
| Device relay action | action default sonnet | Per action; start sonnet@medium | Does a cheaper row clear each action's schema, posture, and senior-output bar? |
| HTTP target | Opaque external configuration | Not variable inside LightTrack | Compare explicitly versioned endpoint configurations as distinct targets |

## How to read

The floor is the cheapest row that clears Maya, Tomáš, Priya, and Daniel where they are bound.
The ceiling is the largest measured business-value improvement from a premium row. A premium
that changes no must-pass verdict is waste. Cross-family claims need two judge families or a
human spot-check. All hypotheses here are theoretical until a dated benchmark session links them
to fixed inputs and raw captures.
