---
name: Elena García - finops-product-ops
type: tiger/character
maps_to: ["[[online-freeform-judge]]", "[[benchmark-freeform-evaluator]]", "[[structured-rubric-judge]]", "[[batched-rubric-judge]]", "[[pairwise-preference-judge]]", "[[benchmark-model-generation]]", "[[benchmark-http-target]]", "[[benchmark-healing-recommendations]]", "[[gateway-chat-completion]]"]
use_case: [UC1, UC2, UC3]
references:
  - "https://www.finops.org/framework/capabilities/unit-economics/ - relate cloud or AI cost to business outcome units"
  - "https://www.finops.org/framework/technology-categories/ai/ - AI cost allocation, usage, and value context"
---
# Elena García - FinOps / Product Operations Lead

## Who they are / Background / Voice

Elena owns the monthly AI cost review and the product decision that follows it. She distrusts
cheapest-model tables that omit quality, and quality leaderboards that omit volume, cached input,
failed attempts, or unpriced rows. Her voice is commercial and exact: What outcome unit did we buy,
what did a usable one cost, and which cheaper row clears the same bar?

## Jobs to be done

- Reconcile tokens, cached tokens, retries, repairs, and cost to a project/use case.
- Compare quality, latency, and dollars on the same benchmark cells.
- Turn model experiments into a cheapest-sufficient routing recommendation.
- Keep unknown price or usage fields null and visible rather than optimistic zeroes.

## Senior-quality bar

The output states the price date and source, separates estimates from measured spend, includes failed
and repair attempts, and conditions recommendations on quality and volume. It never calls an unpriced
target free or a partial matrix sufficient.

## Time-saved (motivation)

Baseline hypothesis to validate: 120 LLM-less minutes to reconcile and explain a model cost/value
comparison; 30 minutes with LightTrack. The 90-minute saving is a pre-run target.

## Scored acceptance criteria

- [ ] Names the supplied project/use case, model, volume, price date, and accounting unit.
- [ ] Usage and cost reconcile across attempts, repairs, cached input, and successful outputs.
- [ ] Unknown and estimated values are labelled; null never wins as zero.
- [ ] Quality and tail latency sit beside cost for the identical workload.
- [ ] Recommendation is the cheapest configuration that clears a declared quality bar.
- [ ] The analysis is concise enough to drive an actual routing or budget decision.
