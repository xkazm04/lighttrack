# Architect

Heavy-hitter codebase scan for **structural patterns** — both weak ones to upgrade and strong ones to codify. Designed for rare, deliberate, high-effort sessions where the payoff is a class of bugs eliminated, a tech swap landed, or a convention promoted from "tribal knowledge" to "enforced rule."

Adapted for **LightTrack** (Rust workspace) from the personas suite. It uses `context-map.json` (repo root) for area taxonomy, `CLAUDE.md` + `docs/` for conventions, and the lighttrack Obsidian vault for a durable backlog of architectural decisions that span multiple sessions.

## Interaction conventions

Built for parallel CLI control — every user prompt is single-keystroke answerable.

- **Every prompt is a numbered menu.** Numeric input picks the option; **Enter** triggers the default; option `1. other → …` is the deviation lane (free text).
- **Every phase output (intermediate or final) ends with a `Next?` block** of 2–5 numbered next-step actions.
- Multi-finding triages use `<id>=<verdict-number>` syntax (e.g. `1=2 2=1 3=3`); `all=<n>` and `ask` shortcuts are always accepted.
- Long free-text answers are still accepted everywhere; the menu just makes the common case instant.
- When running under a harness with `AskUserQuestion`, render menus through it; defaults become the recommended option.

## Input

### Q1 — Mode

```
Mode? (Enter = scan)
  1. scan      — pick a theme, parallel-agent sweep        ← default
  2. area      — bound the sweep to one area
  3. resume    — drain the backlog (skip scanning)
```

`resume` skips the rest of Input — jump straight to Phase 9.

### Q2a — Theme (scan mode)

```
Theme? (Enter = pick for me)
  1. other → describe (free-form theme; angles auto-picked in Phase 3a)
  2. store-backend-parity     (Store trait × SQLite/Postgres/Firestore drift)
  3. error-handling           (LtError/anyhow boundaries, HTTP error mapping)
  4. api-surface              (handler shape, auth guards, DTO consistency)
  5. data-modeling            (core types, timestamps, ids, serde contracts)
  6. testing-strategy         (conformance suite reach, test placement, gaps)
  7. async-patterns           (tokio usage, blocking-in-async, spawn discipline)
  8. provider-boundary        (engine providers, judge, retry/pool, HTTP clients)
  9. config-and-env           (env vars, defaults, feature flags, dotenvy)
  10. pick for me   ← default (uses Architect/coverage.md staleness)
```

### Q2b — Area (area mode)

```
Area? (Enter = pick for me)
  1. other → type a hint (path fragment, keyword, or context name)
  2. traces      — Trace Observability & Privacy
  3. judge       — LLM-as-Judge Scoring
  4. bench       — Benchmarking & Datasets
  5. profit      — Cost, Revenue & Profit
  6. integrations— Integration Surfaces (MCP, CLI, SDK, responder, relay, collective)
  7. platform    — Platform & Persistence Infrastructure
  8. pick for me   ← default
```

Options 2–7 map 1:1 to the six groups in `context-map.json`. Option 1's free text resolves against context names and `filePaths`. Scan is bounded to that area but still cross-cutting within it.

---

## Constants

- **Codebase reference files:**
  - `context-map.json` — feature map. Resolves area scope and target file lists.
  - `CLAUDE.md` — project working agreement (structure rules, idioms, invariants). Read in full.
  - `docs/ARCHITECTURE.md`, `docs/DECISIONS.md` — the *what* and *why*. Heavily consulted in scan mode.
  - `docs/BENCHMARK_FRAMEWORK.md`, `docs/ROADMAP.md`, `docs/PACKAGING.md` — as relevant to the theme.
- **Vault root:** `C:/Users/mkdol/Documents/Obsidian/lighttrack`
  - `Architect/scans/` — one note per scan run, the synthesis output
  - `Architect/decisions/` — one ADR per accepted decision
  - `Architect/backlog.md` — durable queue of accepted decisions with status
  - `Architect/strong-patterns.md` — load-bearing patterns, kept for codification
  - `Architect/weak-patterns.md` — anti-patterns identified, with affected files
  - `Architect/coverage.md` — themes/areas previously scanned, staleness
  - `Patterns/architect-preferences.md` — distilled rules across runs
  - `Lessons/{date}-architect.md` — append-only self-reflection
- **Categories of finding** — `weak-pattern | strong-pattern | tech-swap | structural-bug-class | convention-gap`
- **Risk** — 1 (low, isolated) … 5 (production-critical surface)
- **Effort** — `s | m | l | xl`
- **Reach** — concrete number: "{N} files / {M} call sites / {K} crates" — never vague.
- **Payoff** — 1 (incremental) … 5 (eliminates a recurring bug class or unblocks a major future)

---

## Coordination — parallel sessions

This working tree is shared with a second session (see CLAUDE.md → Parallel-session coordination). Before Phase 7 (Execute):

- **Leave Postgres-adjacent code alone**: `crates/store-pg/**` and the store-selection block in `crates/api/src/main.rs` belong to the other session. Findings may *reference* them; execution must not edit them without explicit user approval.
- Inspect `git status --short`; classify every dirty path as theirs / yours / in-your-touch-zone (Phase 7c).
- Stage explicit paths only. Never `git add -A` / `.` / `-u`. Never `git stash`, `git reset --hard`, `git checkout --` / `git restore` on paths you didn't author, `git clean`.

---

## Phase 0: Resolve vault path

```bash
VAULT="C:/Users/mkdol/Documents/Obsidian/lighttrack"
[ -d "$VAULT" ] || { echo "No lighttrack vault found. Aborting."; exit 1; }
```

### Bootstrap (one-time per vault)

If missing, create `Architect/`, `Architect/scans/`, `Architect/decisions/`, and seed:

- `Architect/backlog.md` — headers `## Pending` / `## Shipped` / `## Abandoned / Blocked`, status values `proposed | approved | in-progress | shipped | abandoned | blocked`.
- `Architect/strong-patterns.md` — "Load-bearing patterns identified by /architect."
- `Architect/weak-patterns.md` — "Anti-patterns with reach data; each converts into a backlog decision or an explicit 'tolerable for now'."
- `Architect/coverage.md` — "Heatmap of themes and areas scanned, with last-scan date."
- `Patterns/architect-preferences.md` — "Rules upgraded from Lessons/ after 3+ observations."

`Lessons/` and `Perfect/` are shared with other skills — don't recreate or disturb.

---

## Phase 1: Load context & memory

### 1a. Required-file check

`context-map.json` and `CLAUDE.md` must exist; if `context-map.json` is missing, stop and suggest Vibeman refresh.

### 1b. Read in order

1. `CLAUDE.md` — structure rules, Rust idioms, build workflow, invariants. In full.
2. `docs/ARCHITECTURE.md` + `docs/DECISIONS.md` — architecture and recorded decisions (don't re-propose what's already decided; do flag drift from a recorded decision).
3. `context-map.json` — area taxonomy, file lists.
4. `$VAULT/Architect/strong-patterns.md` — avoid re-flagging known strengths as discoveries.
5. `$VAULT/Architect/weak-patterns.md` — what's already on the radar.
6. `$VAULT/Architect/backlog.md` — what's pending or in-progress.
7. `$VAULT/Architect/coverage.md` — staleness signals.
8. `$VAULT/Patterns/architect-preferences.md` — deprioritize finding shapes the user has rejected before.
9. The 3 most recent `$VAULT/Lessons/*-architect.md` files.

### 1c. Snapshot freshness

Warn if `context-map.json`'s `generatedAt` is >30 days old or commits have advanced >200 since.

### 1d. Aging strong-patterns review

For each strong-patterns entry: if `Codification status: noted` AND age > 60 days AND no `Last reviewed` within 30 days → mark **aging**; surface in Phase 5. Already-codified entries (`docs-written`, `test-guard-added`, `lint-gate-added`) are never flagged.

---

## Phase 2: Mode dispatch

Scan → Phase 3. Area → Phase 3 with scope override. Resume → Phase 9.

---

## Phase 3: Parallel scan (scan + area modes)

Spawn **3–5 `Explore` sub-agents in parallel**, each looking at the theme/area from a different angle.

### 3a. Pick the angles

Default angles for a generic theme:
1. **Usage map** — where does this concept appear? Count call sites, group by crate/module. Identify shape variation.
2. **Type/contract** — are types consistent? Trait boundaries respected? Leaky abstractions? serde/DTO drift?
3. **Failure mode** — what happens when this fails? Result/error consistency, recovery, observability, `unwrap()` in library code.
4. **Performance surface** — hot paths, blocking in async, lock scope, N+1 queries, allocation churn.
5. **Test coverage** — tested at the right layer? Unit vs conformance vs API tests? Gaps that hide regressions?

Theme-specific swaps:
- `store-backend-parity` → 1, 2, 5, plus "conformance-suite reach vs trait-method count".
- `error-handling` → 1, 2, 3, 5.
- `api-surface` → 1, 2, 3, plus "auth/validation at the boundary".
- `data-modeling` → 1, 2, plus "schema-vs-type drift" and "timestamp/id discipline".
- `testing-strategy` → 5 (deeply), plus "fixture duplication" and "harness reach".
- `async-patterns` → 1, 2, 3, 4.
- `provider-boundary` → 1, 2, 3, plus "retry/timeout consistency".
- `config-and-env` → 1, 2, plus "default drift across binaries" and "documented-vs-actual env vars".

Area mode: every angle is bounded to the area's `filePaths` from `context-map.json`.

### 3b. Sub-agent prompt template

Each prompt is **self-contained**. Use `Explore` (read-only) for all.

```
You are scanning the LightTrack codebase (Rust workspace, self-hosted LLM
observability + LLM-as-judge tool) at C:\Users\mkdol\dolla\LightTrack for {angle}.

Theme: {theme}
{If area mode:} Scope: only these files/dirs: {area filePaths}
Background: {1 paragraph from CLAUDE.md / docs relevant to the theme}

Specific questions:
1. {question tailored to angle}
2. {question}
3. {question}

Report format (Markdown):
- Files inspected: {list, capped at top 30 by relevance}
- Observed shapes: {distinct patterns found, with file:line examples}
- Inconsistencies: {where shapes diverge — specific files}
- Outliers: {any single file doing it differently from the rest}
- Smell strength: 1-5 (1 = healthy, 5 = active drag)
- Cross-references: {where this angle interacts with other parts of the system}

Sample strategically; report shape, not exhaustive detail.
```

Run all sub-agents **in parallel** (single message, multiple Agent calls).

### 3c. Synthesize

Merge reports. Look for **convergence** (multiple angles → same module = high confidence), **conflict** (strength vs weakness = context-dependent), **surprise** (likely the most valuable finding), and **reach quantification** (every weakness gets a concrete count).

If reports are thin (smell strengths all 1–2), the area is healthy in this theme. **Say so explicitly** — don't manufacture findings to fill a quota.

### 3d. Output structure

0–8 weak-pattern findings; 0–4 strong-patterns worth codifying; 0–2 tech-swap proposals (only when smell ≥4 AND swap unlocks payoff a refactor can't); 0–3 structural-bug-class findings. Cap total at **8**, ranked by `(reach × payoff) / (risk × effort)`.

---

## Phase 4: Surface against existing memory

Cross-check every finding against `strong-patterns.md` (flag conflicts explicitly — they're the most interesting finding of the run), `backlog.md` (merge duplicates, note "re-confirming with new reach data"), `weak-patterns.md` (update reach on existing entries instead of duplicating), and `docs/DECISIONS.md` (a finding that contradicts a recorded decision needs the decision re-opened, not silently overridden).

---

## Phase 5: Present findings

Summary table first:

```
#   Type                   Sev    R   E    Reach                       Title
─   ────────────────────   ────   ─   ──   ─────────────────────────   ──────────────
1   weak-pattern           high   3   m    14 files / 3 crates         ...
4   strong-pattern          —     —   —    5 backends                  ... — codify
```

Then per-finding detail — for weak-pattern / structural-bug-class / tech-swap: Type, Reach, Risk (with what-could-break), Effort (with scan/migrate/test ratio), Payoff (what it unlocks), **Current shape** (2–3 sentences + file:line examples), **Proposed shape** (canonical example or sketch), **Migration plan** (3–7 independently-shippable steps), **Risks** (with mitigations), **Already-on-radar** link.

For strong-pattern: Type, Reach, Why it works, Codification vehicle, Risk-to-losing (concrete bug shape if it drifts).

After new findings, print the **Aging** block from Phase 1d, if any.

---

## Phase 6: Triage

```
For each finding, pick a verdict:
  1. execute now    — implement this one in this session
  2. queue          — accept as backlog decision; defer       ← default
  3. drop           — not worth pursuing
  4. rework         — true gap, wrong proposed shape

Reply `<finding>=<verdict>` space-separated, `all=<n>`, `ask`, or Enter (= all=2).
```

- **execute now** → Phase 7. Recommend only one per session; if the user picks more, warn and ask to queue the rest (allow override).
- **queue** → Phase 8 (stub ADR + backlog).
- **drop** → record `decided: dropped` with reason; pattern-track in Lessons.
- **rework** → ask "what shape would actually fit?"; re-present, or queue as `proposed (needs reshape)`.

Strong patterns (new): `1. codify → Phase 7B | 2. note ← default | 3. drop (do NOT persist)`.
Aging strong patterns: `1. codify ← default | 2. snooze (Last reviewed = today, 30d) | 3. drop (remove entry)`.

---

## Phase 7: Execute (one decision, this session)

### 7a. Branch handling

Default is **commit on the current branch** — this tree hosts multiple concurrent sessions; restrictive branching fights that. Offer `architect/{slug}` branch only for risky migrations meant to be reviewed as a unit; never push toward it. The ADR gives the change its identity, not the branch.

### 7b. Write the ADR first

`$VAULT/Architect/decisions/{YYYY-MM-DD}-{slug}.md`:

```markdown
---
date: {date}
slug: {slug}
status: in-progress
type: weak-pattern | structural-bug-class | tech-swap
reach: "{concrete count}"
risk: {1-5}
effort: {s/m/l/xl}
payoff: {1-5}
branch: architect/{slug} | "(committed to main)"
related_scan: [[Architect/scans/{date}-{theme}]]
---

# {Title}

## Context
{codebase reality today, with file:line examples}

## Decision
{what we're doing, specific about scope}

## Consequences
### Positive
### Negative / risks
### Mitigations

## Rollout
1. {step} — {validation: cargo build -p X / cargo test -p X / clippy}
2. ...

## Acceptance criteria
- ...

## Regression checklist
- [ ] {area still works} — verified by: {how}
```

### 7c. Pre-flight checks

**Do NOT require a clean working tree.** Inspect, classify, coexist:

1. `git status --short` — read every dirty/untracked path.
2. Classify each: **in-flight by the other session** (esp. `store-pg/`, `Cargo.lock`) — leave strictly alone; **pre-existing in your touch zone** — surface to the user (commit first / commit on top ← default / abort); **yours** — normal.
3. Capture validation baselines and record in the ADR:
   ```bash
   cargo build -p <touched crates>      # must succeed
   cargo test -p <touched crates>      # baseline pass/fail
   cargo clippy -p <touched crates> 2>&1 | tail -5   # baseline warning count
   ```
   The metric going forward is *delta vs baseline*, not absolute.

**Forbidden here and at every later phase:** `git stash`, `git reset --hard/--merge`, `git restore` / `git checkout --` on any path, `git clean`, `git add -A/./-u`.

### 7d. Atomic commits per rollout step

For each rollout step: apply → run the step's validation → compare to baseline (no new clippy warnings beyond +5; tests at baseline rate; build green) → fix regressions inline, never stack failing commits, no `--no-verify`/`--amend` → stage **only the paths this step touched** → commit `architect: <step title>` with Co-Authored-By footer and ADR wikilink in the body → record the SHA in the ADR.

Remember CLAUDE.md: `cargo test` does not refresh `target/debug/*.exe` — rebuild before smoke-testing a binary.

### 7e. Final regression sweep

Run full validation on all touched crates; smoke-test against a locally-run API when the change affects a service surface. Walk the ADR regression checklist. **If any item is unverified, the ADR stays `in-progress` with a "needs verification" note** — never claim shipped on code review alone.

### 7f. Update ADR status

All steps committed + checklist passes → `status: shipped`, `commits: [...]`; move backlog entry Pending → Shipped. Partial → stays `in-progress`, records remaining steps.

### 7g. Project invariants — non-negotiable

Any commit must preserve CLAUDE.md's invariants: judge engine unbudgeted; judge provider-configurable; DB-backed prices; fixed-width RFC3339(Nanos, Z) timestamps; MCP diagnostics to stderr, write tools gated, no secret-minting over MCP, MCP stays a thin HTTP client; ≤~300 LOC per file; `main.rs` wiring only; no `unwrap()` on fallible I/O in library code. Never commit `.env` or keys — `git check-ignore .env` before committing.

---

## Phase 7B: Codify strong patterns

For every pattern marked `codify` (multiple per session is fine — they're independent and low-risk).

### 7B.a. Pick the vehicle

```
How should "{pattern}" be codified? Pick one or more:
  1. docs-claude  — append a convention to CLAUDE.md (surfaces in every session)
  2. docs-arch    — append a section to docs/ARCHITECTURE.md or docs/DECISIONS.md
  3. test-guard   — a Rust test that walks the tree / asserts the invariant (fails on drift)
  4. lint-gate    — clippy/rustfmt configuration or a CI grep gate in ci.yml
  5. multiple     — combination (e.g. "1+3")
```

Rule of thumb: code shape → `test-guard` or `lint-gate`; architectural boundary → `docs-arch`; project-wide convention every session must know → `docs-claude`. Cross-file invariants (file LOC caps, "no unwrap in lib code", trait-method-vs-conformance parity) suit a `test-guard` that walks the tree with `std::fs`.

Each vehicle = separate atomic commit, `architect: codify <pattern> ...`. Keep doc sections 10–25 lines with a canonical `file:line` example and the anti-shape to avoid. For test-guards: place under the most relevant crate's tests, clear failure message pointing to strong-patterns entry, confirm it passes on current code before commit. For lint-gates in CI: keep them advisory unless the user says ship-blocker (matches existing ci.yml philosophy).

### 7B.b. Update the strong-patterns entry

`Codification status:` + `Codified: {date}` + `Codification ADR: [[...]]` + pointer to the doc anchor / test file.

### 7B.c. Mini-ADR

`$VAULT/Architect/decisions/{date}-codify-{slug}.md` with frontmatter (`type: codification`, `vehicle`, `parent_strong_pattern`, `commits`) and sections **Why now**, **Vehicle and rationale**, **Rollback**.

### 7B.d. Snooze / drop aging patterns

Snooze → set `Last reviewed: {today}`, `Snoozed until: {+30d}`. Drop → delete the entry, add a one-liner to `Lessons/{date}-architect.md` with the reason. No zombie entries.

---

## Phase 8: Backlog the queued decisions

For every **queue** verdict:

- **8a.** Stub ADR (Phase 7b template, `status: proposed`, sketchy Rollout ok, no commits/branch).
- **8b.** Append to `backlog.md` → Pending:
  ```markdown
  - **[{date}] {Title}** — type: {type}, risk: {N}, effort: {s/m/l/xl}, payoff: {N}, reach: {concrete}
    ADR: [[Architect/decisions/{date}-{slug}]]
    Source scan: [[Architect/scans/{date}-{theme}]]
    Status: proposed
    Notes: {triage input}
  ```
  Sort Pending by `(reach × payoff) / (risk × effort)` descending.
- **8c.** Add/update `weak-patterns.md` entries (First seen / Last seen, Reach + trend, Backlog link, Examples). Strong patterns: write entries only for `note`/`codify` verdicts — **never for `drop`**.

---

## Phase 9: Resume mode

- **9a.** Print `backlog.md` Pending as a numbered table (`#, Date, Title, Type, R/E/P, Reach`).
- **9b.** Ask which to execute (`open N` to read the ADR first; `abort`; Enter = #1).
- **9c.** Refresh the ADR: re-verify file:line anchors, re-count reach, read recent git log on touched files. If anything material changed, present the delta — proceed / reshape / abandon.
- **9d.** Jump to Phase 7c and run 7d–7g normally.

---

## Phase 10: Self-reflection

- **10a.** Batched "why did you drop?" question (skip/Enter = "no reason given").
- **10b.** Append `$VAULT/Lessons/{date}-architect.md`: run stats, triage outcome, drop reasons, which angles produced signal vs noise, synthesis misses, calibration drift, one reusable insight.
- **10c.** Backfill drop reasons into the scan note.
- **10d.** Pattern promotion: 3+ repeated drop reasons across Lessons → propose adding to `Patterns/architect-preferences.md`.
- **10e.** If the run discovered a structural fact future sessions need, add it to `docs/ARCHITECTURE.md` (or CLAUDE.md if it's a working rule), tagged with the run date.
- **10f.** Update `coverage.md` for the theme/area: last scanned, findings per scan, actioned counts, yield density.

---

## Phase 11: Persist the scan

`$VAULT/Architect/scans/{YYYY-MM-DD}-{theme-or-area-slug}.md` with frontmatter (mode, theme/area, sub_agents_spawned, findings counts by type, executed/queued/dropped/reworked ids, adrs_written, commits, branch) and body: 1–2 sentence summary per sub-agent angle, per-finding verdict blocks, strong patterns observed, cross-references.

---

## Phase 12: Final summary

Print the run scorecard (mode, theme, sub-agents, findings by type, triage outcome with ADR links/commits, strong patterns identified/codified/noted/aging-actioned, files updated in vault and repo) and end with:

```
Next?
  1. /architect resume — execute next decision ({Q} pending)  ← default if Q > 0
  2. /architect scan   — fill the queue with a new theme
  3. /perfect          — product-direction companion loop
  4. done
```

---

## Notes on use

- **Cadence** — once a week is plenty. Alternate scan (fill the queue) and resume (drain it); a backlog of 20 pending means the next session should be resume.
- **Coexist with uncommitted work.** Never require a clean baseline; never stash/reset/clean; commit only paths you authored.
- **Conflict signal** — a finding contradicting a vault strong-pattern or a docs/DECISIONS.md entry is the most interesting finding of the run.
- **Drift signal** — 3 consecutive scans with zero resume executions means architect is being used as brainstorming; surface it and ask.
- **Tech swaps are the riskiest** — never propose a swap with reach ≥50 files unless smell strength is 5.
