---
name: Priya Raman - ai-security-privacy
type: tiger/character
maps_to: ["[[online-freeform-judge]]", "[[benchmark-freeform-evaluator]]", "[[structured-rubric-judge]]", "[[batched-rubric-judge]]", "[[pairwise-preference-judge]]", "[[structured-verdict-repair]]", "[[dataset-llm-anonymization]]", "[[gateway-chat-completion]]", "[[responder-error-investigation]]", "[[responder-quality-investigation]]", "[[responder-auto-fix]]", "[[device-relay-action]]"]
use_case: [UC1, UC3, UC4]
references:
  - "https://cheatsheetseries.owasp.org/cheatsheets/LLM_Prompt_Injection_Prevention_Cheat_Sheet.html - separation of instructions and untrusted data, validation, least privilege, and monitoring"
  - "https://www.nist.gov/publications/artificial-intelligence-risk-management-framework-generative-artificial-intelligence - govern, map, measure, and manage GenAI risk"
---
# Priya Raman - AI Security & Privacy Engineer

## Who they are / Background / Voice

Priya reviews model features that ingest customer text, call tools, or modify repositories. She
assumes candidate answers, retrieved text, alert messages, prior model output, and relay payloads can
be hostile. Her voice is calm and threat-model driven: Which channel carried this? What was actually
enforced? Where is consent recorded? What leaves the trust boundary?

## Jobs to be done

- Verify that untrusted content cannot rewrite judge, responder, or action instructions.
- Confirm read-only and edit posture at the child process, not only in the parent configuration.
- Minimize and disclose prompt/output retention and prevent production PII from entering eval corpora.
- Trace every automated mutation to explicit, revocable consent and a bounded local workspace.

## Senior-quality bar

The output distinguishes policy from enforcement, inventories trust boundaries and retained content,
uses least privilege, and records suspicious/degraded states durably. A claim such as safe, anonymous,
or read-only without a testable mechanism fails.

## Time-saved (motivation)

Baseline hypothesis to validate: 90 LLM-less minutes for one prompt/provenance/privacy review;
25 minutes with LightTrack. The 65-minute saving assumes complete evidence and is not yet measured.

## Scored acceptance criteria

- [ ] Classifies every prompt source as authored, machine-derived, or untrusted.
- [ ] Untrusted spans are structurally delimited or otherwise prevented from becoming instructions.
- [ ] Prompt/result capture is redacted, minimized, and tied to explicit retention or report_io consent.
- [ ] Tool, filesystem, auth, and mutation permissions are enforced and observable.
- [ ] Suspicious input, schema shedding, fallback, and missing provenance remain visible downstream.
- [ ] Security conclusions are specific enough for Priya to approve or reject the release.
