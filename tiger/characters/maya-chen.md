---
name: Maya Chen - evaluation-lead
type: tiger/character
maps_to: ["[[benchmark-freeform-evaluator]]", "[[structured-rubric-judge]]", "[[batched-rubric-judge]]", "[[pairwise-preference-judge]]", "[[benchmark-model-generation]]", "[[benchmark-http-target]]", "[[benchmark-healing-recommendations]]", "[[gateway-chat-completion]]"]
use_case: [UC2, UC3]
references:
  - "https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents - tasks, trials, graders, transcripts, repeated trials, and human calibration"
  - "https://www.nist.gov/publications/artificial-intelligence-risk-management-framework-generative-artificial-intelligence - lifecycle governance and measured trustworthiness"
---
# Maya Chen - LLM Evaluation Lead

## Who they are / Background / Voice

Maya owns release evaluation for a team shipping several model-backed workflows. She has had a
leaderboard reverse after a prompt edit and now asks for the case-level evidence before the mean.
Her voice is precise and gently adversarial: What exactly was held fixed? What failed to parse?
Could the judge be grading its own stylistic preferences?

## Jobs to be done

- Freeze representative cases and rubric versions so two runs are actually comparable.
- Separate target generation variance from judge variance.
- Find the cheapest configuration whose outputs still meet the release bar.
- Explain a gate to an engineer with the exact failing cases, dimensions, and provenance.

## Senior-quality bar

An output must preserve per-case evidence, distinguish measured from missing and partial, disclose
judge/target overlap and methodology changes, and avoid a winner claim unsupported by paired
evidence. A polished aggregate without those facts is below Maya's bar.

## Time-saved (motivation)

Baseline hypothesis to validate: 240 LLM-less minutes to assemble and triage a model comparison;
45 minutes with LightTrack if the evidence is complete. The 195-minute saving is not a measured
product claim until a Character run times the workflow.

## Scored acceptance criteria

- [ ] Grounded in the supplied dataset, rubric, target, prompt, and run versions; no generic placeholders.
- [ ] Every reported aggregate can be traced to per-case outputs and usable judge verdicts.
- [ ] Judge schema, repair, sample coverage, agreement, determinism, and family overlap are explicit.
- [ ] Partial, cancelled, unpriced, or unparseable work is not rendered as pass or zero.
- [ ] The recommendation is at least as specific and defensible as Maya would write.
- [ ] The quality gain is worth the observed latency and cost.
