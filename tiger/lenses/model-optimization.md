---
note_type: lens
lens: model-optimization
updated: 2026-09-15
source: Tiger v2.3.0 references/lenses.md
---
# Lens C - Model Optimization

This lens varies model x thinking for each LLM piece and asks:

What is the cheapest configuration that clears every bound must-pass Character's senior-quality
bar, and does a premium configuration materially improve business value?

## Read the piece before pricing it

- [ ] Separate bounded extraction/scalar/schema work from unbounded reasoning, diagnosis, or prose.
- [ ] Compute realized swing after local clamps, weighting, floors, or blends.
- [ ] Identify output caps that collapse the useful effort axis.
- [ ] Identify whether input or output dominates and whether the stable input prefix is cacheable.

### LightTrack levers

- Judge scores are clamped to 0..1 and rubric weighting/floors are local at
  crates/engine/src/judge.rs:81-139,273-310; the bounded scalar is not the whole quality question.
  Reasoning, hostile-input resistance, agreement, and correct ranking remain model-sensitive.
- Structured, batch, and pairwise schemas sharply bound answer shape. Freeform anonymization,
  healing, responder diagnosis/fix, gateway chat, and many relay actions are unbounded.
- Provider/model/effort converges at crates/engine/src/providers.rs:299-370. OpenAI/Gemini
  deterministic paths pin temperature and seed where supported; Claude/Codex CLI determinism is
  best-effort. Unsupported effort must be recorded unavailable, not substituted.
- Batch amortizes stable judge context. General prefix caching was not discovered during init, so
  input-heavy rows should report the cache state their cost assumes.

## L1 theoretical method

Use task shape, Character quality bars, and [[../models]] to pre-register a frontier and place the
current selection as potentially under-, right-, or over-provisioned. Predicted is a label, never a
benchmark result.

## L2 benchmark method

- Hold input, prompt/schema, repository revision, posture, and dataset/rubric versions fixed.
- Run the matrix rows in [[../models]] and capture tokens, latency, cost, raw output, and every unavailable or degraded state.
- Score with two or three blind Character judges. Withhold A/B/C mapping; force a named adjacent-pair ranking; use multiple trials and majority.
- Report quality_delta from the current default and cost_delta per call and per month at measured volume.
- Recommend the floor and ceiling per call site, and watch for lost grounding or hallucination.

## Judging rules

- Quality is judged separately from the model under test.
- Cross-family comparisons require two judge families or a human spot-check; within-family effort comparisons may use one independent judge.
- More effort is not presumed better on prose. No effort upgrade is recommended from length or polish alone.
- If every cell disappoints, investigate prompt framing before buying a larger model.
- Fixtures stress the unbounded subtask and plant a signal the output must catch.
- Where API keys are unavailable, the approved agent harness may run one isolated cell per model/effort; wall clock is the latency proxy and reasoning effort is inferred only from observable output/tokens.

## LightTrack must-pass panel

The floor must clear [[maya-chen]], [[tomas-novak]], [[priya-raman]], and [[daniel-brooks]]
where each is bound. [[elena-garcia]] resolves cost/value ties. A site with no live-compatible
matrix remains theoretical and says why.

## Verdict contract

Findings carry lens: model-optimization, the Character whose bar moved, use_case, model_variant,
quality_delta, cost_delta, and dimension in cost, senior-quality, or trust. Per-site decisions are
keep, downgrade to X, or upgrade to Y and update the engine note, [[../models]], and [[../backlog]].
No benchmark number may be fabricated.
