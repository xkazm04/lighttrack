# Perf+Feature Fix Wave 6 — Eval reproducibility (theme T8)

> 3 commits. Closed the last locally-fixable Critical + 2 Highs + 1 Medium. Baseline preserved:
> workspace 435 passed / 0 failed, 0 warnings (+2 tests). Branch `vibeman/perf-feature-2026-07-16`
> (off `main`, NOT pushed) — 23 commits total across Waves 1/4/2/3/5/6.

## Mental model

An eval product's core promise is **"re-run it, get what you published."** Reproducibility was broken
at three independent layers: the judge sampled nondeterministically, runs didn't record what they
scored, and served prompt versions never linked to the traffic they produced. Each fix closes one layer.

## Commits

| # | Commit | Finding(s) closed | Sev | Files |
|---|---|---|---|---|
| 1 | `bb57d79` | judge-engine feature #1 (no temperature/seed) | High | `engine/src/{providers,judge,pairwise}.rs` |
| 2 | `b46a92f` | prompt-registry #2 (gate ignores version) + benchmark-suites #3 (no pins) | High + Med | `runner/src/{bench,rubric,compare,serve,main}.rs`, `api/src/prompts.rs` |
| 3 | `a832674` | prompt-registry #1 (versions unattributed) | **Critical** | `api/src/{prompts,events,main}.rs`, store dims (SQLite+Firestore), `core/margin.rs` |

## What was fixed

1. **Judge calls request deterministic sampling.** No judge path set any sampling parameter. New
   `generate_deterministic` (temperature 0 + fixed seed for OpenAI/Gemini; the Claude CLI has no such
   knobs — documented residual) used by all three judge entry points (rubric, freeform, pairwise);
   target *generation* keeps provider-default sampling (variety there is the point). A provider that
   rejects a strict feature falls back once to a plain call with a loud log.
2. **The promotion gate is version-aware, and runs pin what they scored.** `maybe_enqueue` already
   tagged the job payload with `{prompt_id, version}`; the runner now threads it (plus `judge_model`,
   `rubric_id`, `dataset_ref`) into every run report via `stamp_pins`, and `promote()` selects the
   newest run that *provably scored the promoted version* — a green run of v3 can no longer clear v9
   for production. Legacy untagged benches keep the old behavior; tagged-but-unscored correctly blocks.
3. **Prompt-version attribution (the Critical).** `GET .../prompts/:name` returns the prompt `id` and
   a ready-to-stamp `tag` (`name@vN`); the documented convention is `metadata.prompt` (mirroring the
   `customer_id` linkage — no schema change). The store dim vocabulary gains `"prompt"` across
   cost/tokens/daily rollups (SQLite + Firestore), and new `GET /v1/costs/prompts` answers "did v4
   cost less than v3 in production?" in one request — proven end-to-end through the wired router.

## Verification

| Gate | Result |
|---|---|
| `cargo check --workspace --all-targets` | clean, 0 warnings |
| workspace tests | 435 passed / 0 failed |
| New pins | version-gate branch matrix (v3-green-must-not-clear-v9 among them); prompt rollup e2e (v3=2 calls/$0.80, v4=1/$0.20, untagged disclosed under null) |

## Patterns established (catalogue, continued)

14. **A verdict is a measurement** — judge/eval calls get deterministic sampling; generation keeps
    sampling variety. Split the entry points rather than flag the shared one.
15. **Provenance rides free-form report/metadata fields** — `{prompt_id, prompt_version}` in run
    reports, `metadata.prompt` tags on events: version attribution with zero schema migrations,
    queryable by the existing dimension rollups.

## What remains (wave-6 tail → followups)

- **scoring-rubrics #1 (High, partial):** run reports now pin `rubric_id`, but per-SCORE linkage still
  writes `rubric: "bench:{name}"` — a proper `rubric_id` column on scores is cross-backend schema work,
  and changing the rubric *string* would break consumers that key on it (alerts, calibration watch,
  enrich). Deferred with that constraint noted.
- Rubric `version` field (immutability + bump-on-change) — same family, same session.
- SDK-side: stamp `metadata.prompt` automatically when a prompt is fetched through a future SDK helper.
