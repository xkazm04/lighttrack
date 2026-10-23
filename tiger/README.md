# tiger/ - the Tiger vault (open me in Obsidian)

This folder is an Obsidian vault and the per-app overlay for the Tiger skill. Tiger certifies
the LLM call sites of LightTrack across three lenses and stores the evidence here so each run
extends the last. Start at [[MOC]].

This vault was initialized on 2026-09-15 with Tiger v2.3.0. Init maps the surface; it does not
run Characters, make paid model calls, or certify findings.

## What's here

- engine/ - one note per semantic LLM prompt family; engine/_expected/ is reserved for jobs the code implies but does not implement
- lenses/ - [[engine-quality]], [[business-value]], and [[model-optimization]]
- [[models]] - model x thinking matrix, repository price snapshot, and theoretical frontiers
- [[_roster]] - five research-grounded Characters and their use-case bindings
- fixtures/ - the fixed-input contract for later live certification
- sessions/ - dated run notes; [[backlog]] - the living, impact-ranked backlog

## Jobs / use cases

| use_case | Job (who, the loop) | What Lens B's grounding audit asks for this job | Judges (>=2) |
|---|---|---|---|
| UC1 observe-govern | An AI-platform operator captures calls, reconciles tokens/cost/margin and privacy posture, detects a breach or quality drop, then investigates and resolves it | Real event/trace identity, provider/model, measured usage and cost, limit or alert evidence, recent comparable history, and explicit degraded/partial state must reach the output and survive in the report | [[tomas-novak]], [[priya-raman]], [[elena-garcia]], [[daniel-brooks]] |
| UC2 evaluate-promote | An evaluation lead freezes representative cases, generates target outputs, applies a stable rubric, compares candidates, and gates promotion | Frozen dataset/version, input and optional reference, target/prompt identity, rubric dimensions/anchors/floors, judge identity, sample/parse coverage, and paired per-case evidence | [[maya-chen]], [[elena-garcia]], [[daniel-brooks]] |
| UC3 route-serve | An application developer sends an OpenAI-compatible conversation, LightTrack chooses a configured route with bounded failover, and the caller inspects the answer and provenance | Full supported conversation, system/developer instructions, tools and response schema, caller use-case, resolved route/target, fallbacks, determinism/schema state, measured tokens/latency/cost | [[maya-chen]], [[tomas-novak]], [[priya-raman]], [[elena-garcia]] |
| UC4 relay-act | An operator leases a private device action, the device resolves local instructions and permissions, executes it, and settles an auditable result | Action version/template, complete task payload, device-owned workspace and posture, schema, budget/timeout, model, result status, fingerprint, and opt-in content disclosure | [[tomas-novak]], [[priya-raman]], [[daniel-brooks]] |

## Use-case hard checks

- UC1: usage and cost are attributed only to the call that produced them; partial, missing, or degraded evidence never renders as a healthy zero or complete diagnosis.
- UC2: every comparison uses the same frozen cases and judge contract; unparseable or partial work cannot become a passing score; the target is not its sole judge.
- UC3: unsupported tools or modalities fail before spend; route, served target, fallback attempts, determinism, and schema degradation remain visible to the caller and telemetry.
- UC4: the cloud never chooses a filesystem path or permission posture; read-only and edit modes are enforced locally; edit is explicit opt-in; prompt/result text leaves the device only under per-action report_io consent.

## Expected kills

None identified at init. Every declared AI job maps to at least one discovered call site.
Roadmap aspirations are not treated as current product promises. A later scan should add an
engine/_expected note only when a declared job step implies a genuinely absent model call.

## Discovery

- Searched Rust sources, manifests, configuration, and documentation for provider SDK/HTTP endpoints, Claude and Codex process invocation, generation/judging wrappers, prompt builders, response decoders, and high-level consumers.
- Followed each production import chain from prompt assembly through provider dispatch and response validation.
- Grouped syntactic callers that share the same prompt, grounding, and output contract into one stable semantic call-site note.
- Excluded tests, fixtures, mocks, generated target/, and documentation examples from the call-site count.
- Current inventory: 15 text prompt families. No image, vision, embedding, or audio generation call was found.

## Model-invocation recipe (Lens C)

- Judge families: select [provider/]model@effort through the runner judge setting. Exercise freeform, rubric, batch, and pairwise paths against the same frozen cases; keep the judge family separate from the candidate family.
- Benchmark generation: declare provider, model, thinking level, and prompt reference in the benchmark target. Run the same frozen dataset across every matrix row and retain per-case outputs.
- Gateway: vary the route target in gateway.toml or use a literal provider/model@effort request model. Keep the full request identical. The gateway currently accepts but ignores generic SDK temperature and max_tokens fields.
- Responder: vary defaults.model in the responder map while holding the alert envelope, enrichment snapshot, repository revision, posture, and budget fixed.
- Device relay: vary model and optional effort in the local action.toml while holding prompt.md, schema.json, task payload, workspace revision, and permissions fixed.
- HTTP benchmark targets are opaque applications; LightTrack cannot vary their internal model or thinking setting. Benchmark those as named external configurations.
- Init performs none of these live calls. Later benchmark runs store raw transcripts only under gitignored session raw directories and commit scored summaries.

## Fixtures

The fixed-input and planted-signal contract is in [[fixtures/README]]. No output fixture was
fabricated during init. The first L1 run may use checked-in deterministic corpus outputs; any
claim about real model quality remains ungrounded until an L2 capture exists.

## Price basis

config/pricing.json:7,15-28 is the app-owned seed price book. [[models]] snapshots its
2026-05-31 verification date and USD-per-million-token rates. Those rates are a repository
baseline, not a claim about public prices on 2026-09-15.

## Fingerprint procedure

Each engine note fingerprints the ordered prompt-builder and schema/decoder source slices named
in that note. Procedure tiger-source-slices-v1 serializes, for each slice, path:start-end plus LF,
then the exact LF-normalized source lines each followed by LF; SHA-256 of the concatenation is
the fingerprint. Dynamic case text is excluded. The device action also records a runtime SHA-256
of each fully rendered prompt in product telemetry.

## Run it

- tiger init remaps the surface and scaffolds this vault; tiger scan diffs it.
- tiger run performs the L1 sweep; adding --l2 permits live calls.
- tiger benchmark SITE executes the model matrix; tiger recall and tiger backlog replay memory.

This vault is committed because it is project memory. Raw benchmark transcripts are ignored;
scored summaries are retained.

## Skill improvement log

- 2026-09-15 proposal, not an installed-skill edit: the ordinary-copy installation at
  .claude/skills/tiger is not directly editable. Clarify the engine-note template so init can
  record last_reviewed as init; no session instead of inventing a session that the init contract
  explicitly says not to run.
