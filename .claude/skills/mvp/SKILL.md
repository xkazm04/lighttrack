---
name: mvp
category: Shipping
memory: project
description: Launch-readiness orchestrator for taking a dev project (typically NextJS) from "advanced WIP" to first market release. Assesses 21 checklist items across 7 chronological phases (Define → Rails → Automate → Harden → Polish → Market → Launch gate), reports an honest scorecard, then walks each phase with batched select decisions and executes accepted work with subagents. Delegates infra to passport-onboard, artifacts to project-populate, security to security-review instead of duplicating them. State lives in a public-safe mvp-passport.json at the target repo root; the skill self-calibrates across runs via state/calibration.md. Invoke with `/mvp [project-root]`.
argument-hint: "[project-root]"
---

# /mvp — first-release readiness loop

You are assessing and driving ONE repository toward its first market release.
Your altitude is the **launch decision** — "can a stranger find this, use it,
and can we live with what happens next?" — not code perfection. The two
sibling skills own lower altitudes and you DELEGATE to them, never duplicate:

- `passport-onboard` — infra wiring (hosting, CI, DB, auth infra, observability)
- `project-populate` — Personas artifacts (context map, features, KPIs)
- `security-review` — the security pass
- `ship-loop` — long-horizon quality convergence (not needed for a single /mvp run)

The checklist is `references/checklist.md`: **21 items in 7 phases P0–P6**,
chronologically ordered because early items define scope for later ones
(P0 value case decides what P4 onboarding leads to and what P5 landing says).

## Target resolution

Argument = project root (default: cwd). All assessment and edits happen in the
TARGET repo. If the target is Personas-managed (`.personas/` dir or the
operator says so), use the memory outbox at the end. Never edit the personas
app repo from this skill unless the target IS that repo.

## The loop: Assess (parallel) → Scorecard → Phase walk (gated) → Certify

### Phase A — Assess (parallel, read-only)

If `mvp-passport.json` exists at the target root, read it FIRST: trust levels
as of `generatedAt` (re-verify only cheap probes), and NEVER re-ask an item
marked `skippedByChoice` — surface it as "skipped on <date>, say the word to
revisit". Also read `app-passport.json` if present — passport-onboard's levels
are authoritative for the delegated items (cicd, deploy, observability, auth
infra); don't re-derive what it already observed.

Spawn 4 read-only assessors IN PARALLEL (Explore-class), one per cluster:

1. **Product & market** — P0 (value-case, monetization) + P5 (landing, seo, legal, analytics)
2. **Rails & automation** — P1 (artifacts, milestone, notes, fleet) + P2 (cicd, deploy, observability)
3. **Engineering floor** — P3 (code-quality, auth, security, design-system, i18n)
4. **Experience** — P4 (onboarding, feedback) + P6 (launch-gate)

Each assessor gets its items' probes from `references/checklist.md` and
returns per item: **level** (🟢 met / 🟡 partial / 🔴 missing / ❔ can't tell
from code), 1–2 lines of evidence with file paths, and 2 realistic paths
forward with ONE recommendation. Assessors read; they never write. For items
that code can't answer (is there a distribution plan? is monetization
decided?), the assessor returns ❔ and the QUESTION goes to the operator in
the phase round — never guess a product answer from code.

### Phase B — Scorecard

One screen before any question: the 7 phases as rows, each item with its
level emoji + a ≤10-word evidence note. State the overall read in one
sentence ("P0–P2 largely green, launch blockers concentrate in P5").

### Phase C — Phase walk (P0 → P6, batched selects)

Walk phases in order. Per phase:

- Intro text states ✓ items (no question wasted on them) and any
  skipped-by-choice priors.
- ONE AskUserQuestion call for the phase's below-target items (max 4
  questions; if a phase needs more, split by theme, never one-per-item).
  Every question offers: **Skip** · path A · path B with **(Recommended)** on
  exactly one · Other is built-in. Options name OUTCOMES ("Landing live on
  the prod domain with the 3 killer features above the fold"), not chores.
- ❔ items become direct questions ("Monetization: free beta / pricing page
  now / skip the decision — recorded either way").
- **Pipeline**: present P(n)'s round as soon as its cluster's assessment is
  in-hand; execute accepted P(n) work while presenting P(n+1) when the work
  is independent. Don't hold the operator for the slowest builder.
- Execute accepted items with parallel subagents (strongest available model
  for real engineering changes). One item = one scoped brief carrying: the
  accepted path, the DONE criterion from checklist.md, repo conventions, and
  the shared-checkout commit discipline (stage only your paths, never `-A`;
  verify staged set matches intent; no `--amend` once concurrent commits may
  exist). Merge briefs whose file scopes collide. Delegated items invoke the
  owning skill's flow (dimension-scoped passport-onboard, etc.) rather than
  reimplementing it.
- Builders self-verify (build/lint/test as available) before reporting; a
  blocked builder reports WHY, never silently drops its item.
- Re-assess touched items with the checklist probes; a phase closes when
  every item is 🟢 or ⚪ skipped-by-choice. An operator may explicitly
  ACCEPT a 🟡 to move on — record it as `acceptedAt: "partial"` in the
  manifest; it resurfaces at the P6 go/no-go.

### Phase D — Certify (P6) + report

P6 is not another work item — it is the ritual: run the smoke probe
(critical path against the production build/URL where one exists), then
present the **go/no-go screen**: all 21 items → final level → skipped/
accepted-partial flags. The verdict line is yours to state honestly:
`GO`, `GO with accepted risks: <list>`, or `NO-GO: <blockers>`.

Before the report, write/refresh **`mvp-passport.json`** at the target root
(public-safe: levels, tool NAMES, booleans — never URLs with tokens, env
values, costs, or local paths):

```json
{
  "schemaVersion": 1,
  "generatedAt": "<ISO date>",
  "generatedBy": "personas mvp",
  "verdict": "go | go-with-risks | no-go | in-progress",
  "phases": { "P0": "green|partial|red|skipped", "...": "..." },
  "items": {
    "<item-key>": { "level": "met|partial|missing|unknown", "tool": "<name or null>", "skippedByChoice": false, "acceptedPartial": false, "note": "<1-liner>" }
  }
}
```

Close with a compact table: item → before → after → skipped(choice/blocked),
plus the exact follow-ups the operator still owns. If dispatched from the
Personas wall, end with one greppable line:
`MVP_RESULT: <verdict>, <n> improved, <n> skipped, <n> blocked`.

## Hard rules

- **Honest levels only.** Every level is observed via a checklist probe or an
  explicit operator answer — never inferred optimism. "Subagent-claimed" is
  not "verified"; re-probe before flipping an item green.
- **Skip is always honored** and appears in the report as a choice, not a
  failure. Product calls (monetization, locales, scope cuts, go/no-go) belong
  to the operator — never auto-decide them.
- **Secrets never move.** Connector choices are names + service types; wiring
  reads env var NAMES. Credentials live in Personas Vault, never in this
  terminal.
- **Additive, convention-following changes.** Read before writing; match the
  target repo's stack and idioms; several small verifiable changes over one
  rewrite. Pre-existing lint noise gets reported, never silently fixed.
- **The value case gates scope.** After P0, any proposed work that serves no
  killer feature and no checklist item is a cut candidate — say so instead of
  building it.

## Calibration (while the skill is being finetuned)

This skill is under active calibration across multiple projects. At the end
of EVERY run, append to the SHARED calibration log at
`C:\Users\kazda\kiro\personas\.claude\skills\mvp\state\calibration.md` — the
canonical copy in the personas repo, NOT a `state/` dir next to this file.
This repo is public and the log carries notes on other projects, so it is
deliberately not vendored here; keep all runs writing to the one shared file
so learning accumulates instead of forking. Append: date, target project,
per-phase
friction notes (questions that landed wrong, probes that misfired, items
that should merge/split/reorder), and one concrete SKILL.md/checklist.md
change proposal. Read the file at the START of every run and apply what it
already learned.

## Memory outbox (Personas-managed targets)

If the target repo is Personas-managed, append 3–8 JSON lines to
`.personas/memory-outbox.jsonl` before finishing (append, never rewrite):
`{"type":"node","kind":"progress|decision|gotcha|fact","title":"≤200 chars","body":"optional","context":"optional context name"}`
— record phase outcomes, operator decisions, and gotchas. Skip silently for
unmanaged repos.
