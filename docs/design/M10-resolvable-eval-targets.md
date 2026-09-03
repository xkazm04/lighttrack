# M10 — Resolvable eval targets: the promotion gate runs the version it certifies

Size XL · gate contract · wave C · contexts: prompt-registry, benchmark-management, judge-engine,
scoring, store-postgres

## Problem
Cutting a prompt version enqueues the linked benchmark with `{prompt_id, version}` as **provenance
only** (`crates/api/src/prompts.rs` ~328-345 → `runner/serve.rs` ~359-371 → `bench.rs` ~85-100
`stamp_pins`). Nothing in the runner fetches prompt content (`grep prompts crates/runner/src` finds
only the tags). Compare mode generates from the *stored* `t.system_prompt` (`runner/compare.rs`
~125-132); rubric/simple modes judge pre-existing `cases[i].output` (`bench.rs` ~241-253,
`rubric.rs` ~110-122). `gate_promotion` (`prompts.rs` ~387-438) then passes or blocks on a run
that merely *claims* the version — the quality-gates "gate that does not see its target".
LightTrack can only benchmark `{provider, model, system_prompt}`, so a team whose quality lives in
a RAG pipeline cannot benchmark their app. The prompt registry is `Unsupported` on Postgres
(`store/lib.rs` ~1100-1131; no `store-pg/src/prompts.rs`), so on Neon the gate does not exist at all.

## Design
1. `crates/core/src/score.rs` (or `bench_target.rs` to respect LOC): `BenchTarget += prompt_ref: Option<PromptRef { name: String, version: Option<u32>, label: Option<String> }>`,
   `kind: TargetKind { Model (default), Http { url: String } }` — serde-defaulted, additive. Add
   the report vocabulary constant `RESOLVED_PROMPT_VERSION` beside `RECURRENCE_KEY`.
2. `crates/api/src/benchmarks.rs::validate_target_matrix`: accept the fields; reject `Http`
   without a URL (https only, no private ranges — reuse/extract the URL vetting the alerts code
   will also need); reject `prompt_ref` naming a prompt not in the project.
3. `crates/api/src/prompts.rs::maybe_enqueue`: keep payload tags, add `prompt_name`;
   `gate_promotion`/`evidence_of` read `report.resolved_prompt_version` and block (409, new
   reason string) when absent or ≠ the version being promoted; `force` still overrides. One
   release of advisory: when the linked benchmark has no `prompt_ref` at all, annotate the
   response with a warning instead of blocking (documented).
4. Runner: new `crates/runner/src/targets.rs` — resolve `prompt_ref` at run start via
   `GET /v1/projects/:pid/prompts/:name?version=|label=`, apply the version override from the job
   payload for every target whose `prompt_ref.name` matches, minimal `{{input}}` substitution else
   system+user; `compare.rs`/`pairwise.rs` call the resolver instead of `t.system_prompt`;
   `bench.rs::stamp_pins` writes `resolved_prompt_version`. `Http` kind: POST
   `{input, expected?, system_prompt?}` with an `X-LightTrack-Signature` HMAC header (secret from
   env `LIGHTTRACK_HTTP_TARGET_SECRET`), read `{output, usage?, latency_ms?}`; route through
   `crates/engine/src/providers.rs` as a `ProviderFamily::Other` adapter that prices from `usage`
   when present else `cost_usd: None` (existing unpriced path). `family_of` for an http target =
   its host (never silently "same family").
5. **Port the prompt registry to Postgres**: `crates/store-pg/src/prompts.rs` + tables
   `prompts`, `prompt_versions` in `schema/postgres/001_init.sql` (self-contained block at the END
   of the file); declare the `Prompts` surface in `PgStore`'s manifest; the conformance `prompts.rs`
   section then runs there under the env gate.
6. MCP `create_benchmark` write schema gains the fields. Docs: `BENCHMARK_FRAMEWORK.md` §2 target
   vocabulary; `CI_GATE.md` promotion section.

## Out of scope
Labels/trust (M11). Canary after promotion (M23). Dataset lineage (M24).

## Gates
`cargo build/test/clippy` for lighttrack-core, -api, -runner, -engine, -store, -store-pg, -mcp;
SQLite conformance; new tests: a version-triggered run reports `resolved_prompt_version`;
`gate_promotion` refuses a run without it; an `Http` target round-trip against a local axum stub.

## Evaluation
Before: 0 version-triggered runs use the version's content; gate passes on a tag; registry 501 on
PG. After: every version-triggered run reports `resolved_prompt_version`; gate refuses runs
without it; `Http` targets benchmarked; `Prompts` declared on PG.
