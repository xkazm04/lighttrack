---
subject: llm-observability/grounded-answer-scoring
project: tracklight
raised_by: intake intake-ragas
source: librarian/sources/2026-09-15-ragas.md
stage: the rubric judge, between a case's input/expected/output and the per-dimension verdict
size: 5 files / ~350 lines / M
status: proposed
---

## Why the scope implies it

The manifest's first `does` line is "score with judges". The traffic those judges score is
increasingly an answer the operator's customer wrote *after retrieving passages*: support
bots, document assistants, anything grounded. For that traffic, the question operators ask
first is whether the answer is supported by what it was given. The rubric judge cannot
answer it. A case has three slots, input, reference and output (`prompts.rs`), and no slot
for the evidence. A "faithfulness" dimension is therefore scored holistically, from whatever
the caller pasted into the input string, on anchors like "0.5 = some claim unsupported".

The registry subject's forces are exactly this tree's: a holistic grounding verdict hides
which claim failed and lets a fluent true paragraph carry an invented sentence. The pipeline
that replaces it adds a second model (the claim cutter) whose error is charged to the
candidate unless it is measured.

The judge's other properties already fit what the subject needs, so the direction adds a
capability without contradicting any of them:

- deterministic dimension kinds
- nonce fencing
- versioned rubrics
- the golden-set trust record

## What the first context contains

- **An evidence slot on a case.** It holds passages as separate, individually fenced blocks,
  not concatenated into the input. It is optional, and absent means the grounding kind below
  refuses by name.
- **A `grounding` dimension kind.** The procedure: cut the output into standalone claims, give
  each claim one supported or unsupported verdict against the evidence, and score supported
  over claims *issued*. A claim with no verdict counts as a failed verdict, never a smaller
  denominator. An output with zero claims is unscored, and it is not scored 1.0.
- **The per-claim verdict list** stored in the score's `detail`, beside the existing
  per-dimension reasoning.
- **A decomposition granularity pin** recorded on the rubric version, so a score made at one
  granularity is never trended against another.

It must **not** absorb:

- **Reference-based claim precision/recall.** It is a second kind with its own direction,
  once the first ships.
- **Per-passage attribution.** It costs claims × passages and belongs to sampled diagnosis,
  not every trace.
- **Retrieval ranking metrics.** They are a builder-side concern.

## The measurable

On a golden set of grounded cases where the human label is "contains an unsupported claim",
compare two dimensions on the same cases and the same judge model: a holistic `faithfulness`
LLM dimension, and the `grounding` kind. The number is kappa against the human label on the
**unsupported** class.

The prediction: the grounding kind raises that kappa. The gain should be largest on the
stratum where one unsupported sentence sits inside an otherwise supported answer.

## What would make this wrong

Holistic and per-claim kappa are indistinguishable on the one-unsupported-sentence stratum,
or the per-claim kind's decomposition misses bring its kappa below holistic. If the second
holds, the cutter is the bottleneck, and the direction should become a decomposition-quality
instrument first.
