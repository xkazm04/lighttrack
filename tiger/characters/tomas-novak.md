---
name: Tomáš Novák - staff-sre
type: tiger/character
maps_to: ["[[online-freeform-judge]]", "[[gateway-chat-completion]]", "[[responder-error-investigation]]", "[[responder-quality-investigation]]", "[[responder-auto-fix]]", "[[device-relay-action]]"]
use_case: [UC1, UC3, UC4]
references:
  - "https://sre.google/workbook/on-call/ - actionable alerts, diagnosis, mitigation, escalation, and playbooks"
  - "https://sre.google/resources/practices-and-processes/incident-management-guide/ - structured incident response and clear ownership"
---
# Tomáš Novák - Staff SRE / AI platform on-call

## Who they are / Background / Voice

Tomáš carries the pager for an internal AI platform. He has learned that a fluent diagnosis with
no timestamp, failing request, or code pointer is slower than raw logs because someone must disprove
it first. His voice is terse and operational: Show the observed failure, the blast radius, the next
safe action, and what would falsify the diagnosis.

## Jobs to be done

- Correlate an alert with exact calls, route attempts, recent changes, and affected scope.
- Produce a read-only, evidence-first diagnosis before any mutation is proposed.
- Keep failover and relay actions bounded, observable, and recoverable.
- Know whether a degraded path served a user and whether retrying will help.

## Senior-quality bar

The output identifies the failing path and concrete file:line evidence, distinguishes cause from
hypothesis, states confidence and risk, and gives a reversible next step. It must not hide a fallback,
schema shed, timeout, or partial result behind a successful-looking answer.

## Time-saved (motivation)

Baseline hypothesis to validate: 45 LLM-less minutes to correlate one AI incident and prepare the
first useful diagnosis; 12 minutes with LightTrack. The 33-minute saving is a target, not observed data.

## Scored acceptance criteria

- [ ] Names the supplied project, alert/call identity, route or action, and observed failure.
- [ ] Leads from measured logs/metrics to cause; configuration is explanatory evidence, not the starting guess.
- [ ] Gives exact file:line or event evidence and a confidence level.
- [ ] Distinguishes retryable, degraded, partial, and terminal outcomes.
- [ ] Proposes the smallest reversible action and states blast radius.
- [ ] Saves enough on-call time to justify its latency and spend.
