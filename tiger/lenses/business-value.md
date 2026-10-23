---
note_type: lens
lens: business-value
updated: 2026-09-15
source: Tiger v2.3.0 references/lenses.md plus UAT init method
---
# Lens B - Business Value

This lens inherits UAT's completion, effort, clarity, trust, missing, time-saved, and
senior-quality dimensions, but judges only what the model generates: scores, summaries,
diagnoses, fixes, recommendations, and relay results. It does not judge surrounding chrome.

## Questions that matter most

### 1. Grounding (N/M)

The denominator is the call site's canonical source list, shared by every Character.
Character-specific context is an addition, never a denominator chosen to make a score look better.

- [ ] IN: each required source reaches the actual prompt, not only a warning or log.
- [ ] OUT: engine/model identity, degradation, and the fields the model moved survive in the durable artifact.
- [ ] Computed-but-not-wired context is named, especially data already fetched or paid for.
- [ ] Repeat workflows carry prior choices and what changed, or explicitly disclose cold re-judging.
- [ ] The use-case row in [[../README]] supplies the job-specific registry, exemplar, journal, and prior-run evidence.

### LightTrack canonical grounding by job

- UC1 observe-govern: event/trace or alert identity; measured provider/model/usage/cost/status;
  limit or quality evidence; recent comparable history; repository revision and change evidence for
  diagnosis; durable degraded/partial state.
- UC2 evaluate-promote: frozen dataset/version; input and optional reference; target and prompt
  identity; rubric dimensions/anchors/floors; judge identity; sample and parse coverage; paired
  per-case evidence.
- UC3 route-serve: supported system/developer and conversation turns; tools and response schema;
  caller use case; resolved route and all attempts; served target; tokens/latency/cost; determinism
  and schema state.
- UC4 relay-act: action version/template; complete payload; device-owned workspace and permission
  posture; schema; budget/timeout; model; result and fingerprint; content-disclosure consent.

### 2. Senior-quality

The output must be at least as good as the bound Character would produce as a senior. Generic
advice, an ungrounded number, or an audit that misses the planted signal fails even when JSON parses.

### 3. Time-saved and trust

Every Character declares LLM-less minutes and with-app minutes as numeric baseline hypotheses.
A run measures the loop and asks whether the Character would stake their reputation on the output:
does it reconcile, is it sourced, and does the same input produce an acceptably stable answer?

## Grounding bar

Every verdict quotes the real prompt template at file:line and at least one real sampled output
from logs, a fixture, a prior vault capture, or L2. With no sample, the verdict is ungrounded -
needs L2 sample; the evaluator does not infer quality from a function name or schema.

Init has no model-output sample and therefore issues no Lens B verdict.

## Run scope

- L1 judges the designed prompt and grounding for a specific Character, citing file:line and prompt text.
- L2 invokes the real model and asserts that the output uses the supplied entity/data, contains no placeholders, clears the senior bar, and is worth measured latency and cost.

## LightTrack Character panel

- [[maya-chen]] tests evaluation validity, case-level defensibility, and deterministic comparison.
- [[tomas-novak]] tests operational evidence, diagnosis, recovery, and failure disclosure.
- [[priya-raman]] tests hostile content, privacy, permission enforcement, and provenance.
- [[elena-garcia]] tests unit economics and cheapest-sufficient decisions.
- [[daniel-brooks]] is the external skeptical buyer testing adoption and audit trust.

## Verdict contract

Findings carry lens: business-value, a required Character and use_case, and dimension in trust,
senior-quality, time-saved, clarity, missing, completion, or effort. Each Character adds a
first-person felt verdict: would I trust this number, ship this output, and wait/pay for it?
