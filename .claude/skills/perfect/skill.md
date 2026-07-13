---
name: perfect
description: Session-after-session product perfection loop for LightTrack. The strongest available model (Fable) directs — it walks context-map.json context-by-context, proposes 5 challenged, high-value directions per context (features, API/UX elevations, significant optimizations), gates them with the user until 10 are accepted, then orchestrates one Opus builder subagent per context in isolated worktrees while making every review/merge decision itself. All state lives in a linked Obsidian vault so any future session resumes the loop exactly where the last one stopped. Invoke with `/perfect [init|propose|build|status|reflect] [context-name]`.
---

# Perfect — the direction-and-delivery loop (LightTrack)

> One model is best at *judgment* — seeing what would make a product excellent, challenging its own ideas, reviewing diffs ruthlessly. Cheaper strong models are great at *execution* inside a well-scoped brief. `/perfect` wires the two together in a permanent loop: **Fable directs, Opus builds, the vault remembers.** Each session moves LightTrack measurably closer to the best observability/judging product it can be; no session ever starts from zero.

LightTrack is a self-hosted LLM observability + LLM-as-judge scoring/benchmark tool — a Rust
workspace (crates: api, engine, store, store-pg, store-firestore, core, runner, mcp, agent, cli,
billing, render, responder, anon). Read `CLAUDE.md` and `docs/` for the working agreement and
invariants; this skill layers the loop on top of them, it never overrides them.

## Roles — Director and Builders

- **Director (the main session — Fable, or the strongest model available).** Owns everything that is judgment: opportunity-scoring contexts, drafting directions, adversarially challenging them before the user ever sees them, running the acceptance gate, writing builder briefs, answering builders' product questions mid-flight, reviewing every diff, deciding merge/redo/drop, running the repo gates, committing, and writing the vault. The Director **never delegates a decision** to a builder and never rubber-stamps a builder's diff.
- **Builders (Opus subagents, `model: "opus"`, one per context).** Each receives a tight brief (direction specs + acceptance criteria + the context's `filePaths` scope + repo-law digest) and implements in its **own worktree**. Builders return a structured report; when they hit a genuine product ambiguity they **return the question instead of guessing** — the Director answers via `SendMessage` and the builder continues.
- **Scouts (Explore subagents, cheap).** Produce the per-context current-state brief the Director synthesizes directions from. Never used for judgment.

## The Obsidian vault — durable loop state

Resolve the vault root (first hit wins), then use `$VAULT/Perfect/`:

```bash
for v in "C:/Users/mkdol/Documents/Obsidian/lighttrack" "C:/Users/kazda/Documents/Obsidian/lighttrack"; do
  [ -d "$v" ] && VAULT="$v" && break
done
# First run: if "Documents/Obsidian" exists but "lighttrack" doesn't, create it.
# Portable fallback: no Obsidian at all → use <repo>/.perfect/ (same schema — still Obsidian-openable).
```

```
Perfect/
  Perfect.md               # HOME / Map-of-Content — always reflects current truth:
                           #   mission, the scored context QUEUE with the CURSOR,
                           #   the ACCEPTED POOL (n/10), shipped ledger headline, link to last session
  config.md                # per-repo overlay: gates to run, worktree recipe, wave size,
                           #   direction sizing rules, cooldown, ## User taste, + ## Skill improvement log
  contexts/<name>.md       # one per context-map context (long-lived, updated in place)
  directions/<slug>.md     # one per direction (long-lived; the atom of the whole loop)
  sessions/<YYYY-MM-DD[-n]>.md  # immutable run records, each ends with a `next:` pointer
```

**Context note** (`contexts/<name>.md`):
```markdown
---
name: <context-map name>        type: perfect/context
group: <group>                  category: ui|api|lib|data|config
opportunity: <0-10>             # value reach × headroom × strategic fit (Director's judgment)
last_proposed: <YYYY-MM-DD|never>   cooldown_until: <date|—>
directions: ["[[<slug>]]", …]
---
## Current state   (scout brief digest + file:line evidence — refreshed each proposal pass)
## Direction history   (proposed / accepted / REJECTED-and-why — rejections are memory too)
## Shipped   (direction → commit SHA → observed effect)
```

**Direction note** (`directions/<slug>.md`):
```markdown
---
slug: <kebab, stable>           type: perfect/direction
context: "[[<context-name>]]"   lens: feature|ux|optimization|robustness|wildcard
status: proposed | accepted | building | shipped | failed | dropped | rejected
size: S|M|L                     # must fit ONE builder session (≲15 files, no cross-context schema break)
proposed: <date>  accepted: <date|—>  shipped: <date|—>  commit: <sha|—>
---
## What & why   (the user value, one paragraph, no fluff)
## Evidence   (file:line of the gap/opportunity in today's code)
## Acceptance criteria   (3-6 checkable bullets — the builder's contract AND the review checklist)
## Risks / non-goals
## Build record   (builder report digest, review verdict, gate results — filled during build)
```

**Session note**: phases run, contexts covered, accept/reject tallies, build outcomes with SHAs, deltas, and **`next: <the exact resumption instruction for the following session>`**.

Vault hygiene: slugs are stable; **update notes, never duplicate**. Subagents may fail to write files in some harnesses — after any parallel phase the Director MUST `ls` the target dir and **backfill missing notes from the agents' returned content** before trusting "written".

## The loop — a vault-driven state machine

Every invocation starts the same way; the vault decides which phase runs.

### Phase 0 — Recall & register
1. Read `Perfect.md` (+ last session's `next:` pointer). If missing → run **init** (below).
2. Read `context-map.json`; diff against `contexts/*` — new contexts get notes + a queue slot, removed ones get archived (`status: retired` in frontmatter).
3. Scan MEMORY.md + `docs/ROADMAP.md` for signals that veto or steer directions (shipped arcs, "pending" halves of features, parallel-session territory).
4. Announce the resumption point in one sentence, then go where the state machine points: pool < 10 → **Propose**; pool ≥ 10 (or user said `build`) → **Build**.

### Init (first run only)
1. Scaffold the vault tree + `config.md`. Record the repo gates:
   - `cargo build -p <crate>` for every crate the diff touches (test harness ≠ runnable exe — see CLAUDE.md).
   - `cargo test -p <crate>` for touched crates; `cargo test --workspace` before ending a build session.
   - `cargo clippy -p <crate>` advisory — gate on **no NEW warnings in files the diff touched** only.
   - Smoke-test against a locally-run API (`cargo run -p api` from repo root, `.env` loaded) when the diff changes HTTP behavior.
   Also record: wave size = 3; cooldown = 2 rounds; main branch = `main`.
2. Score every context 0-10 for **opportunity** = user-facing reach × headroom (distance from "perfect", judged from context-map metadata, `docs/*`, and memory) × strategic fit (active arcs in memory). Write the ranked **queue** into `Perfect.md` with the cursor at the top. Don't deep-read code yet — scoring is refined per-context at proposal time.
3. **Hard territory rule**: `Cloud Store Backends (Postgres + Firestore)` is partially owned by a parallel session (`crates/store-pg/**` + the store-selection block in `crates/api/src/main.rs`). Score it low and never brief a builder into `store-pg/**`.
4. Write session note; proceed straight into Propose.

### Phase P — Propose (context by context, until the pool holds 10)
Loop while `pool < 10` and the user hasn't said stop:

1. **Cursor** = highest-opportunity context not on cooldown. **Prefetch**: before presenting context *k*, launch the scout for context *k+1* in the background.
2. **Scout** (Explore, "very thorough", read-only): given the context's `filePaths` from context-map.json → return a current-state brief: what exists, what's rough, dead ends, API seams, perf smells, missing coverage across store backends, with `file:line` evidence.
3. **Draft 5 directions** — one per lens by default: **feature** (new user value), **ux** (API ergonomics / dashboard / CLI / MCP surface elevation), **optimization** (perf/cost/significant simplification), **robustness** (failure modes, observability-of-the-observer, backend parity), **wildcard** (the non-obvious idea a great PM would pitch). Each sized to ONE builder session; a bigger vision ships as its phase-1 slice.
   **Weight the slate by `config.md → ## User taste`** — the lens spread is a starting point, not a quota. Default depth is the *engine*, not the chrome: LightTrack is a backend product, so most directions should be architecture-level (data model, scoring algorithms, ingestion lifecycle, judge/prompt paths, cost structure, store-parity); surfacing (dashboards, render, MCP tools) appears at most once-twice unless the user steers otherwise. Scout prompts must match this depth (trace the full pipeline, not just the handlers).
4. **Challenge before presenting** (the Director argues against itself; a direction that fails any check is replaced, not presented):
   - Does it already exist in code? (scout evidence, not assumption)
   - Was it already proposed/rejected/shipped? (check `contexts/<name>.md` history + MEMORY.md + git log)
   - Does it conflict with a key invariant in CLAUDE.md (judge unbudgeted, DB-backed prices, fixed-width timestamps, MCP write-gating) or with parallel-session territory?
   - Is the value claim concrete — can I name the operator/agent moment it improves?
   - Can one Opus session genuinely ship it behind the acceptance criteria, including the SQLite backend + honest defaults for the others?
5. **Present** the 5 in chat — numbered, each: title · lens · size · one-paragraph why · evidence · acceptance criteria. Then gate with **AskUserQuestion (multiSelect)** — the tool caps options at 4 per question, so use TWO questions in one call: Q1 = directions 1–3, Q2 = directions 4–5 (labels = `N · short title`, description = one-line value claim + size). The user can annotate via "Other" (e.g. `edit 2: …`, `stop`); selecting nothing in both = none accepted.
6. Record outcomes in the vault (rejected ones too, with the user's implied reason — rejections steer future proposals). Accepted → `directions/<slug>.md` with `status: accepted`, pool counter++, context gets `cooldown_until`. Update `Perfect.md` after every context, not at session end — a killed session must lose nothing.
7. **A `none` gate that carries a steer** (the user says what they wanted instead) is a re-scout order, not a rejection of the context: promote the steer to `config.md → ## User taste` if it generalizes, re-scout at the steered depth/angle, and re-propose the SAME context once before advancing the cursor. Never re-present any rejected direction.

### Phase B — Build (one Opus builder per context, Fable decides everything)
1. **Wave plan**: group the pool's accepted directions by context → one builder per context, ≤ `config.wave_size` (default 3) concurrent, and **≤ 3 directions per builder brief** (a 4-direction brief exceeds one agent-session budget — split a bigger context into two sequential builders). Present the wave plan in one screen; on user go (or when invoked as `/perfect build`), execute.
2. **Worktree per builder** — prepared by the Director, NOT via Agent-tool isolation:
   ```bash
   git worktree add .claude/worktrees/perfect-<ctx> -b worktree-perfect-<ctx>
   # Rust: share the main repo's build cache so builders don't cold-compile the workspace.
   # Each builder runs cargo with: CARGO_TARGET_DIR=C:/Users/mkdol/dolla/LightTrack/target
   # (cargo's file locking serializes concurrent builds across builders — expected, not a bug).
   # Copy .env into the worktree root if the builder needs to run lt-runner/api locally (never commit it).
   ```
3. **Brief** each builder (see template below); launch with `model: "opus"`, `subagent_type: "general-purpose"`, all briefs in one message so they run concurrently.
4. **Mid-flight decisions**: a builder returning `DECISION NEEDED: …` gets an answer from the Director via `SendMessage` — product calls, trade-offs, and scope cuts are Fable's alone. A builder that stops without its final report gets one `SendMessage` nudge.
   **Builder-death recovery (session limits WILL kill builders):** the instant a builder dies, `git add -A && git commit --no-verify` a `wip(…)` snapshot **inside its worktree** (isolated tree — add-all is safe there; never-lose-work beats commit hygiene). Then the Director either finishes the work inline (review the WIP diff, complete gaps, split into per-direction commits along file boundaries — same-file hunks may share a commit if the message says so) or re-briefs a fresh builder after the limit resets with "continue from the WIP commit".
5. **Review — the Director earns its title here.** Per builder branch: `git diff main...worktree-perfect-<ctx>` and review against each direction's acceptance criteria, CLAUDE.md law (≤300 LOC/file, wiring-only main.rs, per-domain modules, no `unwrap()` on fallible I/O in libs, `pub(crate)` discipline, tests beside code), the key invariants, and taste. Verdict per direction: **merge** / **redo with notes** (SendMessage, builder fixes in place) / **drop** (`status: failed`, reason recorded). Never merge on "tests pass" alone — read the diff.
   **Docs-vs-code check:** when a diff documents a behavior (contract text, formula, doc comment, OpenAPI-ish description), grep for the code that implements it before merging. A contract describing behavior the code doesn't have is worse than nothing.
   **Backend-parity check:** a new Store capability must land as SQLite implementation + a *stated* stance for Postgres/Firestore (default impl, explicit unimplemented with reason, or full port) — silent SQLite-only drift is a review failure. `store-pg/**` changes are out of bounds; if parity there is required, record it in the direction note as a handoff for the parallel session.
6. **Merge serially**: per direction, `git merge --squash` (or cherry-pick) → ONE atomic commit on main, message `feat(<context>): <direction title>` + the `Co-Authored-By` footer. Stage per-file, verify `git diff --cached --stat` matches intent (foreign pre-staged files → `git restore --staged` them). Run the config gates after each merge; a red gate is fixed inline before the next merge. Before any push: `git fetch origin && git rev-list --left-right --count origin/main...HEAD` — push fast-forwards, rebase only if diverged (parallel session shares this history).
7. **Doc-sync in the same turn**: user-visible changes update the mapped `docs/*` page and, if the change moves an arc tracked in MEMORY.md, update that memory file. If the change alters which files a context owns, update `context-map.json`.
8. **Cleanup**: per worktree — `git worktree remove .claude/worktrees/perfect-<ctx>`, then delete the branch once its commits are on main.

### Phase W — Wrap (every session, even interrupted ones)
1. Update every touched vault note; write the session note with the **`next:` pointer** (e.g. `next: propose — cursor at event-ingestion-query, pool 7/10` or `next: build wave 2 — judge-engine + benchmark-suites remain`).
2. `Perfect.md` headline refreshed: pool count, queue cursor, shipped-total, last-session link.
3. **Reflect on the skill itself**: 2-4 bullets in `config.md → ## Skill improvement log` — what dragged, what the user overrode, what the next round should change. This log is the input for the between-rounds skill revision.

## Direction quality bar (what earns a slot in the 5)

- **Value-first**: names the operator/agent moment it improves; "nice refactor" is not a direction unless it unlocks something.
- **Evidence-backed**: cites today's code (`file:line`), not vibes.
- **One-session-shippable**: ≲15 files, no cross-context schema breaks; else slice it.
- **Novel to the vault**: not shipped, not pending, not previously rejected (unless the world changed — say so).
- **Lens-diverse**: default one per lens; substituting a second entry in one lens requires the Director to say why.

## Builder brief template

```
You are an Opus builder for the `<context>` context of LightTrack, a self-hosted LLM
observability + LLM-as-judge benchmark tool (Rust workspace; axum API, SQLite/Postgres/Firestore
stores, provider-pluggable judge engine).
Work ONLY in this worktree: <abs path>. Your scope is this context's files:
<filePaths from context-map.json>. Touching other contexts requires DECISION NEEDED.
NEVER touch crates/store-pg/** or the store-selection block in crates/api/src/main.rs
(a parallel session owns them) — if your change needs Postgres parity, note it in your
report as a handoff instead.

Implement these accepted directions, one atomic commit each, message `feat(<context>): <title>`:
<per direction: What & why · Acceptance criteria · Evidence file:line · Risks/non-goals>

COMMIT EACH DIRECTION THE MOMENT IT IS DONE AND VERIFIED — never batch commits
for the end of the session. An interrupted session must lose at most the
direction in progress, not everything.

Repo law (non-negotiable — full text in CLAUDE.md, read it first):
- ≤ ~300 LOC per file; split by responsibility. main.rs is wiring only. One module per domain.
- Store trait stays in crates/store/src/lib.rs; backends delegate to per-domain submodules;
  row mappers live beside their domain. Timestamps are fixed-width RFC3339(Nanos, Z).
- No unwrap() on fallible I/O in library code — return Result. pub(crate) unless genuinely public.
- Key invariants: judge engine is UNBUDGETED; judge is provider-configurable; prices are DB-backed;
  MCP diagnostics → stderr only, write tools stay gated behind LIGHTTRACK_MCP_ALLOW_WRITES.
- Tests live beside the code (#[cfg(test)] mod tests). Build with
  CARGO_TARGET_DIR=C:/Users/mkdol/dolla/LightTrack/target cargo build -p <crate>
  (cargo test does NOT refresh the runnable exe — build before smoke-testing a binary).
- Secrets: .env is git-ignored, never commit keys; stage explicit paths, never `git add -A`
  on the main tree (inside YOUR worktree, add-all is acceptable only for wip snapshots).
- Verify before claiming done: cargo build -p + cargo test -p for every crate you touched,
  and drive the actual flow (run the API locally, curl the endpoint) when the change has an
  HTTP surface; report what you COULD NOT verify honestly.

If a product decision is ambiguous, STOP that direction and return `DECISION NEEDED: <question>`
with your recommendation — never guess. Final report format:
per direction → status (done|blocked|decision-needed), commits, files, verification evidence, open risks.
```

## Modes

- **`/perfect`** — resume the loop wherever the vault says it stopped (the default; covers init on first run).
- **`/perfect propose [context]`** — force a proposal pass (optionally jump the cursor to a named context).
- **`/perfect build`** — build now with the current pool even if < 10.
- **`/perfect status`** — read-only: queue, cursor, pool, in-flight builds, shipped ledger, last session. No agents.
- **`/perfect reflect`** — read `config.md → Skill improvement log` + last sessions and propose concrete edits to THIS skill file.

## Guardrails

- **Never stash, never `git add -A` on the main tree** — per-file staging, staged-count check before every commit; the parallel session's untracked work (`store-pg/`, `Cargo.lock` churn) is sacred.
- **Cost discipline**: scouts are Explore-tier; Opus is spent only on accepted work; the Director never re-runs a scout whose brief is < 1 round old (it's in the context note).
- **Honest ledger**: a direction only reaches `shipped` with gates green AND the Director having read the diff; anything else is `failed` with a reason. No silent drops — every accepted direction's fate is recorded.
- **Interruptibility is a feature**: write the vault incrementally (after every context in P, after every merge in B) so a killed session resumes losslessly.
- **The user is the product owner**: the gate is theirs; the Director challenges but never overrides a rejection, and repeated rejections of a lens/context recalibrate the queue scores.
- **Public repo**: `github.com/xkazm04/lighttrack` is public — nothing secret in commits, briefs, or vault-quoted snippets that land in code.
