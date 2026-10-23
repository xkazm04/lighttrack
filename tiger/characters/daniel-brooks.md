---
name: Daniel Brooks - external-vp-engineering
type: tiger/character
maps_to: ["[[benchmark-freeform-evaluator]]", "[[structured-rubric-judge]]", "[[benchmark-model-generation]]", "[[benchmark-http-target]]", "[[gateway-chat-completion]]", "[[responder-error-investigation]]", "[[responder-quality-investigation]]", "[[responder-auto-fix]]", "[[device-relay-action]]"]
use_case: [UC1, UC2, UC4]
references:
  - "https://www.nist.gov/publications/artificial-intelligence-risk-management-framework-generative-artificial-intelligence - risk evidence across the AI lifecycle"
  - "https://www.anthropic.com/engineering/building-effective-agents - simple, composable workflows and explicit agent trade-offs"
---
# Daniel Brooks - external VP Engineering buyer

## Who they are / Background / Voice

Daniel is evaluating LightTrack for a regulated SaaS company and has no loyalty to its architecture.
He needs self-hosting and data ownership, but will not adopt another observability layer that creates
an opaque score nobody can defend in a customer review. His voice is skeptical and outcome-led:
Can my team operate this at 02:00? Can I show why a decision was made? What happens when the model is wrong?

## Jobs to be done

- Prove value on one production workflow without surrendering prompts or customer data.
- Show that evaluation gates and incident diagnoses are reproducible and contestable.
- Understand operational load, failure states, and the boundary of automated action.
- Decide whether the tool saves senior engineering time rather than moving toil into curation.

## Senior-quality bar

The output connects a concrete workflow to defensible evidence, names limitations and ownership, and
offers a reversible adoption path. Marketing language, hidden fallbacks, model-defined truth, or an
autonomous edit without explicit consent are immediate failures.

## Time-saved (motivation)

Baseline hypothesis to validate: 180 LLM-less minutes for a proof-of-value evidence review;
45 minutes with LightTrack. The 135-minute saving is an adoption hypothesis, not a testimonial.

## Scored acceptance criteria

- [ ] Uses the supplied company/workflow evidence and identifies who owns each next action.
- [ ] A score or diagnosis can be traced to input, model, prompt/rubric version, and raw evidence.
- [ ] Self-hosting, content retention, failure, fallback, and automation boundaries are explicit.
- [ ] The recommendation includes a narrow reversible trial and measurable success criteria.
- [ ] Output quality is senior-grade enough to share with security and engineering leadership.
- [ ] The credible time saving exceeds deployment and curation overhead.
