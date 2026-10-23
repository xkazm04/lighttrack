# Tiger fixture contract

Init defines the fixture shape but does not synthesize model output. A later run should pin every
fixture to a repository commit, dataset/rubric version, Character, and use case.

| Call-site family | Fixed input and planted signal | Required durable capture |
|---|---|---|
| Freeform and structured judges | Known-good, known-bad, borderline, confident-wrong, verbosity-padded, and nonce-boundary attack outputs against one frozen input/rubric | Rendered prompt digest, output, parsed verdict, judge/model/effort, schema and determinism state, injection flag, tokens/cost/latency, parse/repair count |
| Batched judge | Same cases in rotated positions, plus one missing/duplicate case id and one injection attempt | Per-case attribution, batch size, order, failures, agreement, amortized accounting |
| Pairwise judge | Equal pair, clear winner, verbosity-only variant, and both A/B orders | Both raw order verdicts, combined winner, position-bias flag, full accounting |
| Benchmark generation | One frozen case per workload stratum with target/prompt version fixed; include a hard case that differentiates models | Candidate raw output and target provenance before independent judging |
| HTTP target | Signed and unsigned local fixtures, malformed JSON, empty output, missing usage, and retryable response | Exact endpoint configuration, response contract fields, retry result, honest unpriced state |
| Dataset anonymization | Synthetic email/card/key plus names, organization, location, and identifier whose task meaning must survive | Before/after only with synthetic data, placeholder counts, method, semantic-preservation verdict |
| Healing recommendations | Aggregate report with a deliberately weak dimension and a warning that advice must not ignore | Raw bullets and a grounding check against every supplied aggregate |
| Gateway chat | Multi-turn system/user/assistant exchange, response schema, tool call/result, ignored SDK knobs, and forced first-seat failure | Request/response, route and served target, attempt chain, schema/determinism, telemetry parity |
| Responder investigations | Synthetic alert and repository revision with a planted causal diff; include malicious error/reasoning text | Read-only posture, prompt/output, cited file:line evidence, confidence, budget/timeout |
| Responder auto-fix | Disposable clean worktree with one minimal seeded defect and a rejecting test | Proposed diff, branch, commit/test outcome, rollback state; never run on the primary checkout |
| Device relay action | Synthetic task payload with required/missing placeholders across generate/read-only/edit modes | Action version, rendered-prompt fingerprint, posture, schema result, report_io disclosure state |

Maya owns evaluation discriminability, Tomáš operational diagnosis, Priya hostile and privacy
fixtures, Elena cost/value accounting, and Daniel the external adoption/trust read. No fixture
may contain production secrets or personal data.
