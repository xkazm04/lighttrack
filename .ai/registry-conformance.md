# Registry conformance — software-engineering

contributor: mkdol-dev-box · audited: 2026-08-24 · drained: 2026-08-24 (waves 2 and 2b) · bundle:
`software-engineering` (146 subjects)

14 subjects selected against this repo's real surfaces — the three store backends behind one `Store`
trait, the judge/benchmark engine, the ingest clients, the CI ladder, the MCP server, the PII scrub,
and the doc/manifest surfaces. 97 techniques judged. Every technique file was read before the row
was written; the slug name is not the contract.

Statuses: `followed` (the codebase realizes it — cited), `partial` (some of it — what is missing is
cited), `deviation` (it applies and the code contradicts or lacks it), `n/a` (a selected subject's
technique that genuinely does not apply here, with the reason), `deferred` (it applies, and an
operator has taken and dated a product decision that settles it the other way — the note names the
decision; engineering does not implement around it).

A deviation is a finding, not a shame. The audit found 14 deviations in 97 rows.

**Wave 2 (2026-08-24, later the same day) addressed 10 of the 14**: six became `followed`, four
became `partial` with what is still missing cited. Six rows that were `partial` were promoted to
`followed` besides. Four deviations were left, each with the reason.

**Wave 2b (2026-08-24, the same day again) drained all four.** Two of them had been recorded as
`out-of-budget` rather than blocked, and design work being in scope this wave is what made them
reachable. `deviation` is now empty. One row moved to a fifth status:

`deferred` — neither `followed` nor `deviation`. A product decision an operator has taken and dated,
which engineering must not quietly re-litigate by implementing around it. One row carries it:
**`embedded-db` / storage-accounting-and-pruning**, *deferred (operator 2026-08-24: keep expired data
until the production dynamic resolves; revisit retention after)*. Retention stays UNBOUNDED on
purpose; no pruner exists and none was built. The half of that technique which is not a product
decision — measuring the growth, stating the policy where the disk is measured, and reclaiming space
losslessly — landed in full, and the row cites it. Nothing in this product can now delete a user's
history, which is precisely what makes the deferral safe to leave standing rather than a gap.

Every drained item stays listed under its wave's `## Drained` heading with the commit that fixed it
and the seeded-failure proof that the new check can go red, so history stays legible.

Status counts across the 97 rows after wave 2b: `followed` 41 · `partial` 41 · `n/a` 14 ·
`deferred` 1 · `deviation` 0.

Per-subject tallies after wave 2b — every selected subject is `deviations=0`:
`test-harness` deviations=0, deferred=0 · `embedded-db` deviations=0, deferred=1 ·
`docs-sync` deviations=0, deferred=0 · `quality-gates` deviations=0, deferred=0 ·
`data-access` deviations=0, deferred=0 · `job-coordination` deviations=0, deferred=0 ·
`cost-metering` deviations=0, deferred=0 · `eval-harness` deviations=0, deferred=0 ·
`tracing` deviations=0, deferred=0 · `telemetry-pii-redaction` deviations=0, deferred=0 ·
`mcp-tools` deviations=0, deferred=0 · `supply-chain` deviations=0, deferred=0 ·
`repo-manifest-standard` deviations=0, deferred=0 · `pipeline-authoring` deviations=0, deferred=0.

**Zero deviations is not zero work.** 41 rows are still `partial`, each citing exactly what is
missing, and those are the honest queue — the backlog below is ranked from them.

## Fixed in the audit wave

Three deviations named by the harvest wave were verified and closed in the same commit series as
this audit:

- **`test-harness` / `out-of-graph-artifacts`** — `clients/python` and `clients/typescript` shipped
  runnable suites no CI job invoked. Added `clients-python` and `clients-typescript` jobs mirroring
  `clients-rust`, plus a ship-inventory→job table in the workflow header (the technique's written
  interim where the inventory gate does not exist yet).
- **`quality-gates` / `policy-projection`** — `CONTRIBUTING.md` published clippy/fmt as "advisory —
  pre-existing debt" while `ci.yml` had both blocking, and omitted `clients-rust` and `cargo-deny`
  entirely. The table now matches the enforced policy, names `ci.yml` as the authority, and
  `.ai/manifest.yaml`'s `controls` block was corrected in the same change.
- **`quality-gates` / `blocking-by-input-determinism`** — the cargo-deny gate's own TIGHTEN-IT-WHEN
  was actionable, so the invocation was split: `deny-policy` (`check bans licenses sources`,
  deterministic given `Cargo.lock`) now **blocks**; only `deny-advisories` stays advisory, on its
  Monday cron. The decision is recorded at the job with the axis named and the promotion dated.

## Conformance map

| subject | technique | status | evidence |
|---|---|---|---|
| test-harness | fixture-economics | followed | crates/store/src/conformance.rs:24 fresh unique project id per run; seeds go through the `Store` write path (`insert_event`), never raw SQL |
| test-harness | flake-lifecycle | partial | the two `#[ignore]`d tests still carry a written reason and an on-demand invocation, and still have no register, owner or expiry — though `bench.rs` now states what it is and is not (a one-off A/B of store shapes, explicitly not the lane), and docs/harness/soak-lane.md writes down the quarantine protocol for the lane's own criteria (quarantine never delete; owner + entry date + failure signature; scheduled review with two exits) with a standing `Quarantined criteria: none` line so an empty set is visible rather than absent. This wave found the failure mode the absence of one hides: the six OTLP tests had been RED since 2026-08-08 — a fixture pinned to the calendar constant 2026-08-01 aged past the 7-day ingest skew window — so `cargo test --workspace`, a blocking check, had been failing for over two weeks on an untouched tree with nothing tracking it. Fixed at the root (crates/api/src/otlp/tests.rs anchors its fixture to the run, resolved once per process), but a register would have caught it in August |
| test-harness | history-driven-partitioning | n/a | one runner per suite, no split across parallel workers — the technique's own "do not pay for a split before it is needed" |
| test-harness | isolation-lanes | partial | CI gives each backend lane a fresh service container / emulator (.github/workflows/ci.yml:88, :127); locally the PG lane runs against the developer's own database by design (conformance.rs:6 "safe against a non-empty database") and there is no clean-environment launcher |
| test-harness | live-app-harness | partial | scripts/smoke.sh drives the built artifact over its real HTTP surface with product-level steps and asserts the effect landed (cost priced from the book), wired at .github/workflows/docker.yml:73 and release.yml:63 — but both are tag/dispatch-only, so no PR is ever gated on the assembled product |
| test-harness | long-lane-certification | followed | the store soak lane, on its own clock: `.github/workflows/soak.yml` (nightly 03:20 UTC + dispatch) runs `crates/store/tests/soak.rs` against criteria DECLARED BEFORE the runs that judge them (docs/harness/soak-criteria.json, read via `include_str!` so a renamed file is a build error). Percentile bounds, not averages, and the trend judged separately from the endpoint — `write_p95_drift_ratio` is the slope measured WITHIN the second half, so the opening bucket's warm-up cannot masquerade as degradation. Each run emits an artifact (criteria + measured series + verdict + lane health), uploaded with 90-day retention, because the unit of value is the sequence. Lane health is asserted BOTH WAYS on every run: earned green on the known-good build, and planted red — a deliberate latency injection that must fail `write_p95_drift_ratio` specifically, since a lane that fails for the wrong reason certifies nothing. First green 2026-08-24, enforced. Deliberately NOT a job in ci.yml: a certification is not a gate, and ci.yml's job names are what branch protection and gate_table_guard.rs read. The short form runs inside `cargo test --workspace` asserting only that the lane is alive and still fires on its plant — not a skip, because a lane wired in and never exercised is the pathology that hides in plain sight |
| test-harness | negative-control-tests | partial | crates/store/src/conformance.rs:514 asserts against the wrong-default answer and names it in the message ("default would return all 3") — the recorded-proof half; no test records an edit-run-revert mutation proof, and the recovery paths have none |
| test-harness | out-of-graph-artifacts | followed | .github/workflows/ci.yml:189/:219/:237 — one job per detached artifact (rust, python, typescript), each naming its own manifest and scoping its own cache, plus the ship-inventory→job table in the workflow header (fixed this wave) |
| test-harness | platform-quirk-absorption | partial | every quirk is absorbed with its reason attached in ci.yml (rusqlite `bundled` so no system libs, libssl-dev/pkg-config for native-tls, Java 21 because the emulator refuses 17, the emulator readiness poll that `::error::`s rather than proceeding) — but in copy-pasted job steps, not one wrapper that is the only door, so a local run inherits none of it |
| test-harness | suite-partitioning | partial | membership is by location (one `tests/` file per backend crate) and each suite is its own CI job with its own environment; but the two env-gated suites exit 0 having checked nothing locally (crates/store-pg/tests/conformance.rs:17) and nothing reconciles the denominator |
| quality-gates | blocking-by-input-determinism | followed | .github/workflows/ci.yml:342 `deny-policy` blocks (deterministic given Cargo.lock), :368 `deny-advisories` stays advisory (RUSTSEC feed), each with the axis and the promotion date written at the job (fixed this wave) |
| quality-gates | chokepoint-tag-registry | partial | the load-bearing confinement rule — "the MCP server is an HTTP client only, never a direct DB client" (CONTRIBUTING.md:127) — is enforced by the package boundary rather than by convention: crates/mcp/Cargo.toml declares no `lighttrack-store` dependency, which is the stronger buy the technique names; the sibling rule "no `unwrap()` on fallible I/O in library code" has no check at all (no clippy.toml, no `[lints]`, no crate-level lint attrs) |
| quality-gates | false-positive-economics | followed | the ambient red is gone: `cargo update -p h2` cleared RUSTSEC-2026-0258 (0.4.14 → 0.4.19) and `deny-advisories` is green, with dependabot now proposing the bumps so a future finding has a remediation path rather than a permanent orange. The one heuristic gate added this wave was measured before it was armed — the gitleaks rule set was driven over the whole tree and history, its single false positive fingerprinted, and the repo's own key rule written to match the full `lt_<8hex>_<64hex>` shape rather than the `lt_` prefix precisely to keep precision definitional |
| quality-gates | gate-laddering | followed | `scripts/gates.sh` is the one script, and it is not a copy: crates/core/tests/manifest_guard.rs fails the build if any capability in the manifest's `controls.ciHardPass` is absent from it or spelled differently, comparing the COMMAND rather than the label. `.githooks/pre-commit` (staged secret scan) and `.githooks/pre-push` (the blocking set) wire it in; the rung is at push, not commit, so a WIP commit stays legal and the commit-stage secret scan is not trained away. `LIGHTTRACK_SKIP_GATES=1` is the deliberate escape hatch, preferred to `--no-verify` for exactly that reason |
| quality-gates | gate-liveness | partial | the Firestore job still asserts its instrument before testing, and every gate added this wave carries a recorded seeded-failure proof in its commit message (the gate table flipped to advisory; the depth cap made to forward; the lease fence removed; a control naming a missing capability; a weakened gate command; both client suites run with LIGHTTRACK_JOURNAL=0) — the technique's "break it, watch it fail, restore" as a written artifact. Several guards also pin their own parsers against fixtures so they cannot decay into comparisons of two empty sets. Still open: `cargo test` reports success on a zero-test run, and the proofs live in commit messages rather than in a re-runnable form |
| quality-gates | hook-hygiene | partial | `.githooks/` now ships a pre-commit (staged-diff secret scan) and a pre-push (the blocking gate set), both committed 100755 so a POSIX clone can actually run them, both fast-exiting when their engine is missing, and both with a named escape hatch. Not automatic: installing them is still one manual `git config core.hooksPath .githooks` per clone (documented in CONTRIBUTING.md), and nothing verifies a given machine has done it |
| quality-gates | policy-projection | partial | both projections now match the enforced policy and each names ci.yml as the authority (CONTRIBUTING.md:148, .ai/manifest.yaml:49 — fixed this wave), but both are still hand-maintained copies with nothing deriving or checking them |
| quality-gates | ratchet-design | n/a | no metric is currently un-zeroable — the fmt/clippy debt was retired outright and the gates promoted to plain blocking, which is this technique's own graduation endgame rather than a ratchet |
| quality-gates | severity-by-construction | partial | the exit-code path carries severity for every blocking job and `continue-on-error: true` stays an explicit, visible neutralizer; the advisory lane now has a remediation path (dependabot) and is green rather than permanently orange. Still missing: no notification or issue is raised when an advisory job goes red, so the Monday cron's findings depend on someone opening the Actions tab |
| quality-gates | unmeasurable-criteria | partial | skips are loud and name the remedy (crates/store-pg/tests/conformance.rs:17) and the binding rung measures unconditionally — the defensible shape; but no skip is counted or rendered, so a local `cargo test --workspace` green never says two backends were not asked |
| data-access | batching-and-n-plus-one | partial | membership-set batch reads follow the empty/bound rules (crates/store/src/sqlite/scores.rs:121 `scored_event_ids`; crates/store/src/sqlite/events.rs:483 `attach_models` batches via one IN-list), but there is no chunking helper under the parameter ceiling and no query-count regression test |
| data-access | cross-driver-invariant-parity | partial | one shared parity suite runs against real SQLite always and real Postgres/Firestore when env-gated (crates/store/src/conformance.rs:24, crates/store-pg/tests/conformance.rs:11, crates/store-firestore/tests/conformance.rs:11) with a unified conflict vocabulary (conformance.rs:108); but `scope_expr` is independently redefined per driver instead of derived from one authority (crates/store/src/sqlite/events.rs:726 vs crates/store-pg/src/events/usage.rs:20) |
| data-access | layering-rules | followed | rusqlite/sqlx usage is confined to crates/store and crates/store-pg (zero matches elsewhere); crates/store/Cargo.toml depends only on lighttrack-core; `Store` methods (crates/store/src/lib.rs:496-1069) return owned domain types, never a connection or cursor |
| data-access | query-construction | followed | `build_predicates` (crates/store/src/sqlite/events.rs:166) binds every value via `Box<dyn ToSql>`; `scope_expr` (events.rs:726) uses an identifier allowlist with the fixed-literal reason stated at events.rs:724; metadata JSON is always bound, never interpolated (events.rs:206) |
| data-access | repo-testing | followed | conformance runs against real embedded SQLite (crates/store/tests/sqlite_conformance.rs) and real Postgres/Firestore via the env gate; schema is built through the production migration path (`schema::apply`, crates/store/src/sqlite/mod.rs:148); fresh in-memory instance per test |
| data-access | row-mapping | followed | the shared `COLS` constant and the degrade-visibly blob path are unchanged, and the silent third option is gone: `parse_enum(column, value)` returns `Result` and names both, symmetric with `parse_ts`. The policy is chosen per column exactly as the technique requires — `Provider`/`Operation` keep their explicit quarantine variants (`#[serde(other)]`) and absorb the unknown, while `status`, `redaction` and the limit-rule vocabularies surface it, because their defaults were the ones that read as "fine" (a corrupt status became a SUCCESSFUL call; a corrupt redaction became "store raw payloads"). All three backends updated |
| data-access | transactions-and-units-of-work | followed | `insert_events_checked` wraps the batch in one transaction with per-item survivability (crates/store/src/sqlite/mod.rs:207); Postgres admission takes `pg_advisory_xact_lock` first and nests each item in its own SAVEPOINT (crates/store-pg/src/admission.rs:107) |
| embedded-db | connection-pooling | partial | sizing and RAII checkout are right, with a reader/writer split (crates/store/src/sqlite/pool.rs:28,:117; mod.rs:88), and acquisition-wait time IS now recorded — `pool.acquire` and `write.lock.wait` are their own metric families, never folded into query time (crates/store/src/sqlite/metrics.rs), so pool saturation is visible at `GET /v1/storage/status`. Still open: `acquire()` blocks forever with no bound or exhaustion error (pool.rs:84) |
| embedded-db | db-self-instrumentation | followed | crates/store/src/sqlite/metrics.rs keys by TABLE OR OPERATION FAMILY — a closed `DbOp` vocabulary, never statement text — with pool acquisition and the write-lock wait as their OWN keys, excluded from operation time because the remedies are disjoint. Slow lines are per-family and calibrated for a local store (10 ms for indexed point reads, not a networked database's 100 ms), and every slow count is served with `slow_over_ms` beside it. `rows_written` separates "the query got slower" from "the table got bigger", and is null for reads rather than zero. All three consumers exist: the warn channel (rate-limited `tracing::warn!` on the API's own structured log, with SUPPRESSION COUNTED — a rolled-over window emits the suppressed count and the worst suppressed duration), the maintenance gate (passes are their own family), and the pull surface `GET /v1/storage/status`. The instrument does not use the database — in-memory atomics plus a bounded ring, pinned by a test asserting the table set is unchanged after 400 instrumented operations |
| embedded-db | extension-lifecycle | n/a | no loadable extensions, custom functions, collations, or virtual tables anywhere under crates/store/src/sqlite/ — only rusqlite built-ins |
| embedded-db | journal-and-durability-modes | partial | WAL is requested *and asserted by reading the pragma back*, with pre-WAL databases upgraded in place (crates/store/src/sqlite/mod.rs:117,:173; tests_concurrency.rs:186) — the technique's "the promised mode silently reverts" case is closed; but `PRAGMA synchronous` is never set or asserted anywhere |
| embedded-db | quiet-window-maintenance | followed | crates/api/src/storage.rs: an activity gauge counting in-flight requests at the router's front door over ALL routes (a long analytical read holds a WAL snapshot and is exactly the work a checkpoint must not compete with), with a Drop token so a panicking handler cannot leave it permanently busy and silently switch maintenance off. The gate is two conditions — gauge zero AND the minimum interval elapsed — and the ladder is quiet → quieter (past a staleness bound, reduced chunk) → escalated, whose bounds are stated as HARMS IN BYTES (journal over 64 MiB, a quarter of the file reclaimable) rather than as elapsed time. Passes run as resumable chunks with the gauge re-read between them and the write lock released before that re-read. Every pass is recorded with trigger, gauge reading, duration, work done and outcome, and DEFERRAL IS AN OUTCOME: ran / nothing_to_do / deferred / failed are counted separately and `last_run` is null until a pass runs. The gate is a pure `decide()` so the failure modes that matter — a gate that never opens, one that always opens, a hard bound that is secretly a clock — are tested without a store or a runtime |
| embedded-db | single-writer-holder-discipline | n/a | the deployment stance is explicitly one API process per SQLite file (docs/ARCHITECTURE.md:107), and the CLI talks to the API over HTTP (crates/cli/src/http.rs) rather than opening the file |
| embedded-db | storage-accounting-and-pruning | deferred (operator 2026-08-24: keep expired data until the production dynamic resolves; revisit retention after) | the PRUNER is the deferred half and deliberately does not exist: `MaintenanceRequest` carries no pruning parameter, so no code path in this product can delete a user's history — which is what makes the unbounded-retention decision safe to leave standing. Everything else the technique asks for landed. Accounting: `Store::storage_report()` reports every table AND every index as its own object (row count, bytes, share, largest first), each byte figure carrying its predicate — `dbstat.pgsize` is PAGES ALLOCATED, not bytes of live rows, and the two diverge by exactly the reclaimable space, which is what lets the report answer its own follow-up question "will anything shrink the file?" — with an unmeasurable figure rendered `null` and its reason rather than a measured-looking zero, plus the WAL sidecar the engine's own accounting cannot see. Reclamation as the separate, evidence-triggered act the technique names: chunked `PRAGMA incremental_vacuum`, triggered by the reclaimable share crossing a threshold rather than by schedule, new databases created `auto_vacuum=INCREMENTAL`, and an older file's report saying outright that it cannot reclaim incrementally, naming the offline remedy AND what it costs in free disk. The retention decision itself is carried IN the report payload, dated, so an operator reading their disk reads the policy in the same breath (docs/ARCHITECTURE.md §12). The one pre-existing pruner is unchanged (collective.rs:65 `purge_before`) and is other instances' contributions, not this instance's history |
| job-coordination | job-observability | partial | crates/api/src/jobs.rs:90 exposes list/get from the store, but no operator surface renders holder and lease deadline or sorts anomaly-first, and no transition history backs any job beyond the raw columns (schema/sqlite/001_init.sql:141) |
| job-coordination | job-state-machines | followed | a closed vocabulary and terminal classifier (crates/core/src/job.rs), claim/cancel as single conditioned writes, and — as of this wave — `finish_job` conditioned too, in all three backends: `status NOT IN (terminal) AND (fence IS NULL OR claimed_at = fence)` (sqlite/jobs.rs, store-pg/src/jobs.rs, store-firestore/src/jobs.rs read-compare-commit under an `updateTime` precondition). A refused finish returns the typed `JobFinish::NotHeld { status, claimed_at }` (crates/core/src/job.rs) → HTTP 409, so the loser is told what beat it; the invariant is pinned for every backend in crates/store/src/conformance.rs `job_leases` |
| job-coordination | lease-renewal | followed | `claimed_at` is the lease and the fencing token: `renew_job_lease` is one conditioned write in all three backends, `POST /v1/jobs/:id/renew` answers 409 on loss, and the runner renews on a TIMER at TTL/3 (crates/runner/src/serve.rs `renew_every`, never per case) carrying liveness only, so a stall in the work cannot stall the heartbeat. The renewal's result is read: on loss the run stops at the next case boundary rather than continuing as a zombie. TTL resized from 600 s to 120 s — detection latency, not job duration (crates/runner/src/cli.rs, crates/api/src/jobs.rs `default_stale_secs`). Not yet done: an explicit release on clean shutdown, and a startup sweep (see terminal-state-recovery) |
| job-coordination | step-position-and-resumability | partial | `Job.progress` is free text, not a structured step id plus plan version (crates/core/src/job.rs:39), and a stale reclaim restarts the whole benchmark from case 0 (crates/runner/src/bench.rs); restart is at least recorded as a lineage fact via `stale_reclaims` + `JOB_ERROR_WORKER_LOST` (job.rs:32, sqlite/jobs.rs:75) |
| job-coordination | terminal-state-recovery | partial | the terminal set has only three members — done/failed/cancelled (crates/core/src/job.rs:84) — with no `expired` verdict, and recovery of stale `running` jobs happens only lazily as a side effect of the next `claim()` (crates/store/src/sqlite/jobs.rs:75), never at process startup |
| cost-metering | budget-enforcement | partial | crates/api/src/events.rs:355 is a real chokepoint with correct guard ordering (validate before durable insert) and rich refusal reporting (events.rs:235); but crates/core/src/limits.rs:184 states plainly that enforcement blocks *ingest recording*, not the provider spend ("inline pre-call blocking still requires the future gateway/proxy mode") — the runner's own spend does have real pre-call gating (crates/runner/src/budget.rs:107) |
| cost-metering | preflight-estimation | partial | crates/runner/src/budget.rs:20 uses nominal fixed token constants (not calibrated from ledger history), :38 an advisory pre-flight estimate, :97 a live hard ceiling checked before each paid call; no estimate-vs-actual calibration loop exists |
| cost-metering | price-tables | followed | one authority: crates/runner/src/util.rs `price_gen_cost_checked` now delegates to `PriceBook`, so the runner resolves date-suffix families, batch/flex variants and prompt-length tiers exactly as ingest does. `(0.0, false)` is preserved deliberately — unpriced is zero WITH the flag, never a phantom cost. A test pins the two against each other over the three resolutions the old string scan could not do; `Provider::from_wire` gives &str callers the shared vocabulary, and the consequence (a price row with a provider outside that enum is unreachable on every path) is recorded at the fixtures it changed |
| cost-metering | reversible-debit-and-settle | n/a | the product meters post-hoc against ingested usage with a ceiling (crates/core/src/limits.rs), not a prepaid balance debited before generation; crates/billing/src/{stripe,polar}.rs only normalize revenue webhooks — no debit, reversal, or wallet mechanism exists |
| cost-metering | spend-attribution | followed | crates/api/src/events.rs:82 server-stamps `api_key_id`/`customer_id` (unforgeable) at the one ingest chokepoint alongside trace id, name and model; crates/core/src/margin.rs:17 defines an `UNATTRIBUTED` bucket so untagged cost lands there rather than being dropped (margin.rs:297 test) |
| cost-metering | spend-observability | partial | dashboards/grafana/dashboards/lighttrack.json carries total cost, cost over time, and cost by project/provider/model, over sorted rollups (crates/render/src/costs.rs); but there is no outlier or most-expensive-calls view, no failed-call-spend series, and none of the self-health panels (default-priced share, unattributed share, estimator drift) that would tell an operator whether the total is trustworthy |
| cost-metering | usage-ledgers | partial | schema/postgres/001_init.sql:28 writes one row per call including `status=error`/`timeout`, and `cost_usd` is nullable so it is never a phantom zero (crates/core/src/event.rs:196); but `input_tokens`/`output_tokens` are `NOT NULL DEFAULT 0` (schema:38), so an unmeasured failed call is indistinguishable from a genuine zero. The retention half is no longer undeclared: the policy is unbounded by an explicit, dated operator decision (2026-08-24), stated in docs/ARCHITECTURE.md §12 and carried in the storage report's own payload, and the ledger's growth is measured per table at `GET /v1/storage/status` |
| eval-harness | assertion-vs-judgment | followed | crates/engine/src/judge.rs:225 scores deterministic dimensions locally at zero cost and :260 calls the judge only for LLM dimensions (k=0 for an all-deterministic rubric); :364 keeps per-dimension verdicts rather than one blended score; :356 returns `Err` on an unparseable or absent sample rather than a confident 0.0 |
| eval-harness | certification-levels | partial | crates/runner/src/gate.rs:16 declares distinct exit codes for passed/regressed/no_baseline/partial/aborted/cancelled — a promotion-shaped verdict — but this is a single empirical tier against a statistical baseline; no cheaper structural level gates entry to it |
| eval-harness | comparison-modes | followed | crates/engine/src/pairwise.rs:60 runs mirrored-order pairwise with disagreement collapsing to Tie plus a position-bias flag; crates/runner/src/compare.rs:34 keys each cell by target×case with skipped cells spelled differently from errored ones; crates/runner/src/gate.rs + stats.rs add the significance-gated absolute baseline comparison |
| eval-harness | eval-economics | partial | a bounded worker pool (crates/engine/src/pool.rs:12, crates/runner/src/util.rs:40) and a live ceiling with a pre-flight estimate (crates/runner/src/budget.rs:38) exist; but there is no product-level mock-execution mode with a marked artifact, no declared-lifetime cache for scenarios or judge verdicts, and the pre-flight estimate is printed advisory rather than refusing an over-ceiling run |
| eval-harness | judge-stability | followed | crates/engine/src/judge.rs:56 pins temperature 0 and a fixed seed; docs/CALIBRATION.md + crates/core/src/calibration.rs re-score a golden anchor set on a schedule (`--watch`/`--drift-threshold`) and persist κ / Pearson / MAE / RMSE as history; judge.rs:404 reports self-consistency `agreement` as a repeatability floor |
| eval-harness | scenario-design | followed | crates/core/src/dataset.rs:5 gives each dataset a stable id, a version and a `frozen` flag ("frozen datasets are immutable so runs stay comparable"), and DatasetItem:28 carries `source_event_id` provenance for captured reality plus free-form tags |
| tracing | cross-boundary-propagation | followed | clients/python/lighttrack/client.py:199 takes trace/span/parent ids as explicit envelope fields rather than thread-locals; crates/api/src/events.rs:69 `normalize_ids` and crates/core/src/trace.rs:16 `normalize_trace_ref` case-fold W3C ids identically across the OTLP and SDK doors so one trace cannot fracture between them |
| tracing | raw-record-viewers | partial | crates/render/src/events.rs:114 `payload_block` renders input/output in a fenced block and redaction happens upstream at capture; but crates/render/src/md.rs:191 `trunc` appends an ellipsis with no stated original size (the technique's "mark the cut"), there is no JSON-aware rendering, and it is a static markdown surface with no search-within-record |
| tracing | span-model | partial | identity fields are first-class (crates/core/src/event.rs:116), duration is derived rather than stored (crates/core/src/trace.rs:79), and absent-vs-zero is handled strongly for cost (`unpriced_spans`, trace.rs:194); but there is no closed-vocabulary `kind` for operation species — every span is implicitly one LLM call — and payloads live inline on the span (event.rs:163) rather than being referenced from a raw-record store |
| tracing | synthetic-and-estimated-traces | n/a | no trace or span is ever reconstructed from totals or logs; crates/core/src/trace.rs builds a trace only from real captured `LlmEvent`s |
| tracing | trace-capture | partial | the crash hole is closed in both clients that had it: opening a span writes a durable breadcrumb (clients/python/lighttrack/journal.py, clients/typescript/src/journal.ts), every exit path retires it, and the next client on the same journal directory re-reports what is left as `status=error` + `lighttrack:unsettled-span` with "tokens and cost unknown, not zero" — never a fabricated clean call; the auto-instrument path mints its span identity at call START so the breadcrumb and the settled event are one span. Two gaps remain, both stated in the READMEs: recovery needs a later client to see the same directory (a container rescheduled onto fresh storage is uncovered — the server-side write-at-open was rejected because the SQLite usage cache folds by rowid and cannot see an in-place update, which would make spend caps wrong), and clients/rust has no span type at all, so its builder still emits only on `send()` |
| tracing | waterfall-rendering | partial | crates/render/src/traces.rs:172 gets the structure right — tree depth, distinct status glyphs, a first-class truncation line (:85), and a wall-vs-compute-time distinction (:72); but position and duration are printed as text (`@120ms +80ms`, :188) on no shared time axis, so "find the long pole at a glance" is not achievable from any surface in the product |
| telemetry-pii-redaction | correlation-preserving-redaction | followed | crates/api/src/collective/identity.rs:17 derives `"c-" + SHA256(api_keys.id)[..12]` — stable across restarts because it keys off the persisted id rather than a per-process salt, carries a kind prefix, and states its collision budget |
| telemetry-pii-redaction | denylist-plus-pattern-pass | partial | only the pattern pass exists (crates/anon/src/lib.rs:29 fixed-order regex rules) applied recursively to every string leaf of input/output (crates/api/src/redact.rs:253 `scrub_value`); there is no field-name drop pass, so a free-text personal field with no shape — a name, an address — is never caught |
| telemetry-pii-redaction | emit-site-inventory | followed | every field a caller can write is now dispositioned and the inventory is stated in both crates/api/src/redact.rs:1 and SECURITY.md: `input`/`output`/`error`/`tags`/`name`/`source`/`metadata` are scrubbed, and the five accounting keys inside metadata (`api_key_id`, `customer_id`, `product_id`, `cost_source`, `pricing_mode`) pass through with an individual "passed, because…" — they are join keys, and rewriting `customer_id` would collapse every customer whose id looks like an email into one bucket rather than protect anyone. Tested both ways (crates/api/src/redact.rs tests) |
| telemetry-pii-redaction | hook-coverage-gaps | followed | one shared chokepoint (`prepare_event`, crates/api/src/events.rs:31, "Both front doors pass through here") runs `apply_policy` + `redact_event` for single, batch and OTLP ingest alike, and the separate relay path calls `redact_event` too (crates/api/src/relay.rs:296) — no per-runtime copy that skips scrubbing |
| telemetry-pii-redaction | redact-at-the-cap | followed | the walk is bounded on all four axes (depth 12, breadth 512, 32 KiB per string leaf, 20k nodes) and at every cap it DROPS rather than forwards, marking the hole `<UNSCANNED: <cap>>` — deliberately spelled unlike the `<EMAIL>`/`<SECRET>` markers so "nothing sensitive here" and "I could not look" never read alike, and logged as a blind spot. The scrub is wrapped: a panic discards the payloads instead of falling back to the unscrubbed original, and is logged at `error`. The cycle guard and exotic-value branch are recorded as genuinely inapplicable (`serde_json::Value` is an acyclic tree with enumerated variants). Non-vacuity proved by making the depth branch forward |
| telemetry-pii-redaction | redaction-invariants-as-tests | followed | crates/anon/src/tests.rs is this technique verbatim: a serialized-absence property suite over generated PII (:271), a clean-survival case against over-redaction (:359), idempotence as the strongest no-leak check (:290), and regression cases pinned from real corruptions (:390); crates/api/src/redact.rs:284 additionally asserts the positive, so a disconnected scrubber fails |
| mcp-tools | authentication-and-scoping | n/a | `lt-mcp` is stdio-only, spawned as a child by its single caller (crates/mcp/src/main.rs:1); the technique's applicability condition — reachable by more than its own parent process — does not hold |
| mcp-tools | client-integration | partial | the portion this repo controls is clean: the committed `.mcp.json` spawns the binary via an argument array with no shell and no embedded secret, and enforced-mode keys are added by the operator rather than minted by an installer; connection custody, name-collision handling and the consent seat belong to the MCP host, outside this repo |
| mcp-tools | orchestration-to-tool-migration | n/a | `lt-mcp` is a from-scratch wrapper over existing HTTP endpoints; no orchestration pipeline was migrated onto the tool surface |
| mcp-tools | server-composition | followed | listing and dispatch stay colocated per domain with a parity test, and both ways of losing the single stdio session are closed: the HTTP client sets a 30 s timeout (`LIGHTTRACK_MCP_TIMEOUT_SECS`, refusing to start rather than falling back to an untimed client), and the dispatch door catches panics into an in-band `isError` result carrying the panic's own message on stderr — never stdout, which is the JSON-RPC channel. The guard is factored so it is tested without an API to talk to (crates/mcp/src/tools.rs tests) |
| mcp-tools | tool-schema-design | followed | verb/object names scoped to one vocabulary, enums for closed sets (crates/mcp/src/write.rs), a declared `outputSchema` per tool (crates/mcp/src/schemas.rs), a clean two-channel split between protocol errors (`rpc::send_error` -32601/-32602, main.rs:84) and in-band `isError` results, and cursor pagination with explicit more-results markers (crates/mcp/src/rpc.rs:33) |
| mcp-tools | transport-selection | followed | pure stdio JSON-RPC (crates/mcp/src/main.rs:26) and crates/mcp/Cargo.toml carries no HTTP-server dependency — matching "serves one application on one machine → standard streams" |
| mcp-tools | untrusted-result-handling | partial | captured input/output — which can be adversarial content from the observed LLM's own conversations — is returned to the calling agent in a generic Markdown fence (crates/render/src/events.rs:114) with no "this is data, not instructions" provenance framing; the project's own stronger per-call nonce fencing for judge prompts (docs/DECISIONS.md D10) was never extended to this boundary |
| supply-chain | archive-extraction-safety | partial | deploy/install.sh:47 extracts into a `mktemp -d` quarantine with a `trap` cleanup and moves out only the four named binaries (an implicit payload assertion), but there is no checksum verification of the downloaded tarball and no size or entry-count budget; deploy/install.ps1:12 is weaker — `Expand-Archive` straight into the final bin directory with no quarantine at all |
| supply-chain | dependency-policy-gates | followed | deny.toml implements all four clauses against the full graph (`targets = []`, deny.toml:9); .github/workflows/ci.yml:342 runs the lockfile-deterministic half as a blocking job; the `ignore` list (deny.toml:23) is empty but for a commented template, so no unexpiring exceptions exist |
| supply-chain | permission-manifest-scoping | followed | deny-by-default `permissions: contents: read` at ci.yml:57; docker.yml:15 scopes to exactly `contents: read` + `packages: write`; release.yml grants `contents: write` at top level but the `verify` job re-scopes down to read (release.yml:95) |
| supply-chain | scheduled-deep-analysis | followed | two jobs on the Monday cron, both separated from their commit-triggered halves because their inputs move without the repo: `deny-advisories` (RUSTSEC feed) and — added this wave — `gitleaks (latest rules, advisory)`, which runs the NEWEST upstream rule set over the whole history so a detection pattern published today can find a token pushed last spring, while the blocking scan stays pinned and deterministic |
| supply-chain | secret-scanning-architecture | followed | the binding rung is named and unskippable: `gitleaks (secrets)` (.github/workflows/ci.yml) blocks on every PR/push over the FULL history on a pinned engine; `.githooks/pre-commit` scans the staged diff and ANNOUNCES a skip when gitleaks is absent rather than failing a fresh clone. Armed after driving the rule set over the whole tree and all 241 commits: one false positive, fingerprinted in `.gitleaksignore` with its rationale (never a path exemption), no baseline file because the repo has never committed a credential. `.gitleaks.toml` adds the repo's own `lt_<8hex>_<64hex>` key shape; proved to fire on a synthetic key while the documented `lt_your_key_here` placeholder is allowlisted |
| supply-chain | update-automation-review | followed | .github/dependabot.yml watches every ecosystem this repo SHIPS — the workspace, the detached `clients/rust`, `clients/typescript`, and the workflows themselves — grouped so a one-maintainer project reviews one PR a week rather than fifteen, majors kept separate. The two ecosystems deliberately NOT covered (the stdlib-only Python client; the gitleaks and rustc pins, which live in no manifest) are written down as decisions with their own update signals, so the gap is a choice rather than an oversight |
| docs-sync | catch-up-markers | followed | three markers, one per surface family and beside the surfaces each describes — docs/catchup-marker.json, .ai/catchup-marker.json, clients/catchup-marker.json — each carrying the four things: an anchor commit + date, an explicit covered list, a first-class skip list whose entries carry a `kind` (`frozen-archive` and `not-in-this-pass` are different debts and must not read alike), and a baseline note. They are unflattering on purpose: `docs` covers 3 and owes 15, `clients` covers 0 and owes 4, which is the state the repo is actually in. `flagged` carries six cross-boundary obligations per-change enforcement cannot gate, each with its exit condition. crates/core/tests/catchup_marker_guard.rs makes them load-bearing — every file under a family's declared surfaces must be in exactly one of covered/skipped, so "full pass" is a predicate rather than a claim and a doc added tomorrow fails the build until it is dispositioned — and it rejects PREDICTIONS mechanically, which is this technique's own cautionary tale turned into a check. scripts/docs-catchup.sh scopes the next pass and refuses loudly (exit 2, "CANNOT DETERMINE RANGE") on a missing or unparseable marker rather than defaulting to either extreme |
| docs-sync | checked-vs-skipped-denominators | partial | the catch-up markers give three surface families a real denominator: crates/core/tests/catchup_marker_guard.rs walks each family's declared surfaces and fails when any file is in neither the covered nor the skipped list, and `sh scripts/docs-catchup.sh` renders covered / owed / frozen counts per family — so "a pass ran" always arrives with how much it did not reach. Still partial in the technique's own shape: no docs drift SCANNER exists whose findings carry a checked-vs-skipped count, so the discipline covers what a pass claims, not what a scan examined |
| docs-sync | coupled-surface-inventory | partial | the mechanical half is closed for the surfaces that had drifted: both projections are now checked in CI (gate_table_guard.rs, manifest_guard.rs) and each names its authority at the site. Still partial for the reason the row gave: the inventory is three tests plus prose, not one declared map with per-surface slots, so a fourth coupled surface added tomorrow joins nothing and is noticed by nobody |
| docs-sync | cross-repo-drift-detection | n/a | all documented source and its docs live in one tree; the only cross-repo relationship is the knowledge-bundle pointer (.ai/manifest.yaml:70), which is consumption, not a documented system living elsewhere |
| docs-sync | dated-corrections | followed | docs/DECISIONS.md:54 corrects the D9 claim in place, dated, with a measured description of what changed; ci.yml:278, :319, :336 and :365 extend the same discipline into the gate definitions themselves (the toolchain-float correction, the split's promotion, the verified-green record, and the live `h2` finding) |
| docs-sync | doc-rot-detection | partial | three detectors now run on every `cargo test --workspace` — the gate-table guard, the manifest guard, and the catch-up-marker guard — each comparing actual claims against a source of truth rather than a timestamp, which is the stronger form. The third adds the coverage axis the other two lack: a document that exists but is dispositioned by nobody fails the build. Still deviating in general: nothing compares docs/'s PROSE against the code, so a document that is listed, current-looking and wrong (ARCHITECTURE §5's BigQuery cloud tier, flagged in the docs marker) is still found by a human or not at all |
| docs-sync | same-change-enforcement | partial | two coupled surfaces are now enforced at the merge stage by the ordinary test gate: crates/core/tests/gate_table_guard.rs fails if CONTRIBUTING's gate table drifts from ci.yml's job names or blocking grades, and manifest_guard.rs fails if `.ai/manifest.yaml` drifts from the shipped spec or from `scripts/gates.sh`. Both force the doc edit into the same change as the source edit. Still deviating in general: nothing collects doc debt for the rest of the tree — these are two specific couplings, not a mechanism |
| docs-sync | source-as-data-without-the-app | n/a | documentation here is plain markdown; there is no in-source typed documentation registry for the read-source-as-data problem to arise over |
| docs-sync | source-doc-mapping | partial | three couplings are declared IN CODE rather than in prose, as `include_str!` pairs a test compares (ci.yml ↔ CONTRIBUTING's gate table; .ai/manifest.yaml ↔ its spec; manifest capabilities ↔ scripts/gates.sh) — a coupling that cannot silently stop existing, since a missing file is a build error. The catch-up markers add the declared-map half on ONE side: each family declares roots + extensions and the guard validates coverage against the actual doc tree. Still deviating on the other side, which is the one this technique is about: nothing maps SOURCE globs to the docs that describe them, so a crate with no documentation coupling is still invisible |
| repo-manifest-standard | capability-not-tool-vocabulary | followed | .ai/manifest.yaml:19-35 names capabilities (build, test, lint, typecheck, format-check, conformance, audit-policy, audit-advisories, smoke, test-client-*) with only the invocation string naming a tool; controls cross-check cleanly against every capability name |
| repo-manifest-standard | generated-from-provenance | partial | unchanged on the provenance fields themselves (`generatedAt`/`generatedFrom` with no generator identity or version), but the file is no longer unchecked: crates/core/tests/manifest_guard.rs validates it against the shipped spec on every test run, and the `controls` projection is bound to `scripts/gates.sh` by command. A generator still does not exist, so the fields still describe a hand-maintained file |
| repo-manifest-standard | must-ignore-unknown | partial | the requirement now has a written specification (.ai/ai-manifest.spec.md §8, including the carry-forward-on-write half and why it is what makes additive evolution possible), and a reader exists that demonstrably ignores what it does not use (crates/core/tests/manifest_guard.rs reads three blocks and steps over `registry`, `knowledge`, `skills` without complaint). Still unverified: no WRITER of this file exists, so carry-forward-on-write is specified but unexercised |
| repo-manifest-standard | pointers-not-embeds | followed | .ai/manifest.yaml:38 `paths:` points at context-map.json, docs/, CLAUDE.md, CONTRIBUTING.md and deny.toml — all committed and all resolving; no content is embedded |
| repo-manifest-standard | semver-additive-evolution | partial | .ai/manifest.yaml:5 correctly pairs a stable identity (`schema: ai-manifest`) with `schemaVersion: 0.1.0`; unverified: the file has only ever been added and revised once, so there is no history against which additive-only discipline can be confirmed |
| repo-manifest-standard | spec-ships-with-artifact | followed | .ai/ai-manifest.spec.md ships beside the manifest and IS the authority (checked: no external source exists, so there is nothing to vendor or drift against — the file states that, and states what happens if one is ever published). It carries the reimplementation clause and states every rule as input + condition + outcome, and it is not decorative: crates/core/tests/manifest_guard.rs enforces C1/C2/C3 on every `cargo test --workspace`, with both a seeded-failure proof and a test pinning its own parsers |
| pipeline-authoring | change-scoped-work-selection | n/a | ci.yml has no `paths:` filters — every job runs on every push and PR; the technique's own "the full plan already fits the feedback budget" clause covers this workspace |
| pipeline-authoring | human-checkpoints-in-a-pipeline | n/a | docker.yml and release.yml only build and publish on an explicit `v*` tag (the human action); there is no CD job deploying to a live shared environment — users deploy self-hosted via deploy/ scripts run outside CI |
| pipeline-authoring | pipeline-plan-auditability | n/a | the three workflows are static hand-authored YAML, not a plan resolved by a generator at run time, so there is no resolved-plan artifact to audit |
| pipeline-authoring | runtime-pipeline-generation | n/a | no bootstrap or generator step exists in .github/workflows/; the plan is a fixed enumerable set (workspace + three client SDKs), matching the technique's "write the file" non-use case |
| pipeline-authoring | step-identity-stability | followed | ci.yml job ids are role-derived and distinct from their human-readable `name:` fields (conformance:68, pg-conformance:88, firestore-conformance:127, test:169, clients-rust:189, clients-python:219, clients-typescript:237, clippy:286, fmt:302, deny-policy:342, deny-advisories:368); docker.yml's `merge` and release.yml's `verify` reference `needs: build` by id, never by position |

## Backlog

Ranked by value. Nothing here is a `deviation` any more — wave 2b drained the last four — so this is
now the ranked queue drawn from the 41 `partial` rows plus the one `deferred` decision to revisit.
Everything drained is listed under its wave's heading below, with its commit.

1. **`embedded-db` / storage-accounting-and-pruning — `deferred`, revisit at productionization.**
   Retention is deliberately unbounded (operator 2026-08-24). Not work to do: work explicitly NOT to
   do until the production dynamic resolves. When it does, what the pruner needs already exists —
   per-table accounting, a lossless reclaimer, and a maintenance ladder to run it on — so the
   remaining design is the policy (age floors, terminal-state allowlists, referential closure,
   dry-run by default), not the mechanism. Carried at the head so the decision is re-read rather
   than forgotten.
2. **`job-coordination` / terminal-state-recovery — recovery is still lazy.** There is no `expired`
   verdict and no startup sweep; stale `running` jobs are still only reclaimed as a side effect of
   the next `claim()`. Wave 2 gave the lease its evidence and its fence, which is the precondition
   for a reaper; the reaper itself is still owed.
3. **`test-harness` / flake-lifecycle — no register.** Wave 2 found what the absence of one costs:
   six OTLP tests had been red since 2026-08-08, on a blocking check, on an untouched tree, and
   nothing was tracking it. The root cause is fixed and wave 2b gave the soak lane a written
   quarantine protocol, but the two `#[ignore]`d tests still have no owner, entry date or expiry.
4. **`quality-gates` / severity-by-construction — advisory findings still reach no human
   automatically.** Both cron jobs are green and both have remediation paths, but nothing notifies
   when one goes red. The nightly soak lane added in wave 2b has the same property, and inherits the
   same gap.
5. **`docs-sync` / source-doc-mapping + coupled-surface-inventory — the source side has no map.**
   Wave 2b closed the doc side: each catch-up marker declares its surfaces and the guard validates
   coverage against the actual doc tree, so a document dispositioned by nobody fails the build.
   Nothing maps SOURCE globs to the docs that describe them, so a crate with no documentation
   coupling is still invisible.
6. **`docs-sync` / the catch-up markers' own owed lists.** The markers landed honest rather than
   flattering: `docs` covers 3 surfaces and owes 15, `clients` covers 0 and owes 4, and six
   cross-boundary obligations sit in `flagged`. That is a real, now-computable queue
   (`sh scripts/docs-catchup.sh`) rather than a finding — and one flagged item is a live drift:
   ARCHITECTURE §5 still describes the cloud tier as BigQuery + Firestore when the shipped backends
   are Postgres + Firestore.
7. **`tracing` / trace-capture — two stated residues.** Journal recovery needs a later client on the
   same directory (a rescheduled container is uncovered), and `clients/rust` has no span type at all.
   Both are written into the row, both READMEs, and now the clients marker's `flagged` queue.
8. **`embedded-db` / connection-pooling — `acquire()` still blocks forever.** Wave 2b made the WAIT
   visible (`pool.acquire` is its own metric family), which is the measurement that makes the bound
   choosable; the bound and the exhaustion error are still absent.
9. **`quality-gates` / gate-liveness — the proofs are in commit messages.** Every gate added across
   waves 2 and 2b carries a recorded seeded-failure proof, which is the discipline; they are still
   prose in a log rather than a re-runnable artifact. `cargo test` also still reports success on a
   zero-test run.

## Drained 2026-08-24 (wave 2b)

The four rows wave 2 left. Each is struck with the commit that fixed it and the seeded-failure proof
that shows the new check can go red. Two operator decisions arrived with this wave and are applied
rather than re-litigated: retention stays unbounded, and authentication/authorization is deferred to
productionization (no row in this repo carried an auth deviation, so only the first bound anything
here).

1. ~~**`embedded-db` / quiet-window-maintenance + storage-accounting-and-pruning — unbounded disk.**~~
   Split by the operator decision, and both halves landed as decided. The RETENTION half is
   `deferred`: no pruner exists, none was built, and `MaintenanceRequest` deliberately carries no
   pruning parameter, so nothing in this product can delete a user's history. The rest is done, in
   `feat(store): account the disk per table, and reclaim what is already dead` and
   `feat(api): the disk surface, and maintenance that finds its window instead of scheduling one`:
   per-object accounting with every byte figure carrying its predicate, lossless chunked reclamation
   (`incremental_vacuum`, with new databases created `auto_vacuum=INCREMENTAL` and older files told
   plainly that they cannot reclaim incrementally), an activity gauge over the whole router, a
   two-condition gate with a quiet → quieter → escalated ladder whose hard bounds are stated as harms
   in bytes, chunked passes that re-read the gauge, and four counted outcomes including `deferred`.
   The dated decision is carried in the report payload and in docs/ARCHITECTURE.md §12, so an
   operator reading their disk reads the retention policy in the same breath.
   Proof: adding a `DELETE FROM events` to the maintenance pass turns the two lossless store tests
   red on the loss itself; weakening the quiet rung's `gauge == 0` to `gauge >= 0` — the exact
   collapse from a two-condition gate to a wall-clock timer — turns two api tests red.
   Found while building it: `PRAGMA incremental_vacuum(N)` yields one empty row per page freed, so
   `execute_batch` reclaimed exactly one page and returned success.
2. ~~**`embedded-db` / db-self-instrumentation — the service has no runtime latency instrumentation
   for its DB path.**~~ Fixed in `feat(store): the store measures itself — per-family latency, a warn
   channel, a report`. Keyed by operation family, never statement text; pool wait and write-lock wait
   as their own keys; per-family slow lines calibrated for a local store; a rate-limited warn channel
   whose SUPPRESSION IS COUNTED; and an in-memory-only instrument, because metrics that wrote to a
   metrics table would double every measured operation and contend for the locks being measured.
   Proof: recording the pool wait under the read key instead of `pool.acquire` — the exact fold the
   technique forbids — turns two store tests red.
3. ~~**`test-harness` / long-lane-certification — no lane on any clock.**~~ Fixed in
   `test(harness): the store's long lane — declared criteria, a nightly clock, a planted red`.
   Its own workflow (not a job in ci.yml, whose names branch protection and the derived gate table
   read), committed criteria, percentile bounds, a trend criterion measured within the second half,
   a per-run artifact with 90-day retention, and both halves of lane health asserted on EVERY run.
   First green: 2026-08-24, enforced, all criteria passed.
   Proof: stubbing `judge()` to return no failures turns the lane red on the planted-red assertion.
   The planted-red check also caught two real defects in the plant itself on its first day — an
   injection placed after the measurement, and one that inflated its own denominator — both written
   into docs/harness/soak-lane.md rather than quietly fixed.
4. ~~**`docs-sync` / catch-up-markers — no marker file.**~~ Fixed in `docs(docs-sync): catch-up
   markers — what the last pass did, and what the next one owes`. Three markers, one per surface
   family, each with an anchor, a covered list, a first-class skip list carrying `kind`, and a
   baseline note. The denominator is enforced (every file under a family's surfaces is in exactly one
   list) and predictions are rejected mechanically — this technique's own cautionary tale turned into
   a check. `scripts/docs-catchup.sh` scopes the next pass and refuses loudly on a missing or
   unparseable marker.
   Proof: an undispositioned `docs/A_NEW_DOC.md` turns the denominator test red naming the file;
   inserting "this cannot happen again" into a baseline note turns the prediction test red quoting
   the phrase; deleting or corrupting a marker makes the script exit 2 with CANNOT DETERMINE RANGE.

### Noticed while draining, not fixed here

- **docs/ARCHITECTURE.md §5 describes a cloud tier that never shipped** — "BigQuery for events/scores
  + Firestore for hot config", while the code has `lighttrack-store-pg` (the cross-cloud default) and
  `lighttrack-store-firestore`, and no BigQuery `Store` implementation exists anywhere. Flagged in
  docs/catchup-marker.json with its exit condition rather than drive-by rewritten: restating the
  cloud-tier story is a bigger claim than this wave verified.
- **context-map.json was not regenerated** for the four source files this wave added, and nothing in
  the repo regenerates or checks it — so its coverage claim decays silently with every new module.
  Flagged in .ai/catchup-marker.json.

## Drained 2026-08-24 (wave 2)

Each item is struck with the commit that fixed it and the seeded-failure proof that shows the new
check can go red. Ordering is the wave's, which followed the dispatch's pinned items first.

1. ~~**`tracing` / trace-capture — the Python client loses crashed calls entirely.**~~ Fixed in
   `fix(tracing): keep the calls that die`. Opening a span writes a durable breadcrumb; every exit
   path retires it; the next client re-reports what is left as `status=error` with tokens and cost
   marked UNKNOWN, not zero. Mirrored in the TypeScript client, which shared the defect, and
   extended to the auto-instrument path, which had it on the code most users actually run. Proof:
   both suites with `LIGHTTRACK_JOURNAL=0` go red on exactly the recovery tests (2 python, 3 ts).
2. ~~**`job-coordination` / lease-renewal + job-state-machines — a long job can be stolen and then
   have its verdict clobbered.**~~ Fixed in `fix(jobs): lease renewal, and a finish that cannot
   clobber a verdict`. `claimed_at` becomes the lease and the fencing token; `renew_job_lease` and a
   conditioned `finish_job` land in all three backends; the runner heartbeats on a timer at TTL/3
   and STOPS when the renewal reports a loss; the TTL drops 600 s → 120 s because it is detection
   latency, not job duration. Conformance-suited (`job_leases`) so every backend proves it. Proof:
   dropping the WHERE clauses from the SQLite finish turns the suite red on the exact clobber.
3. ~~**Blocking gates run on an unpinned ruler.**~~ Fixed in `build(gates): pin the toolchain`.
   `rust-toolchain.toml` is the only place a Rust version appears; every job dropped its toolchain
   action and invokes cargo through rustup.
4. ~~**`supply-chain` / secret-scanning-architecture — no rung at all.**~~ Fixed in
   `security(supply-chain): secret scanning, dependabot, and the h2 advisory`. Blocking full-history
   `gitleaks (secrets)` on a pinned engine, an announced-skip pre-commit hook, an advisory
   latest-rules cron job. Armed after a real triage; proof: a synthetic `lt_` key is caught while
   the documented placeholder is not.
5. ~~**`telemetry-pii-redaction` / emit-site-inventory — `metadata` is un-dispositioned.**~~ and
   6. ~~**redact-at-the-cap — unbounded recursion on an attacker-reachable path.**~~ Both fixed in
   `security(pii): bound the scrub's walk, and disposition every field a caller writes`. Proof:
   making the depth branch forward instead of drop turns two tests red on the leak itself.
7. (now backlog #1 — `embedded-db` unbounded disk, `out-of-budget`.)
8. ~~**`quality-gates` / gate-laddering — no local rung exists.**~~ Fixed in `build(gates): a local
   rung, running the same commands CI blocks on`. Proof: weakening one gate's command in
   `scripts/gates.sh` turns the suite red naming the gate and the command it should have run.
9. ~~**`docs-sync` — nothing enforces or detects doc drift.**~~ The audit's own named first step is
   done: `test(docs-sync): derive the CONTRIBUTING gate table from ci.yml`, plus the manifest guard
   from item 13. Three of the four docs-sync deviations move to `partial`; `catch-up-markers`
   remains (backlog #3). Proof: flipping the fmt row to advisory fails with the exact disagreement.
10. ~~**`supply-chain` / update-automation-review — no dependabot or renovate config.**~~ Fixed in
    the same supply-chain commit, together with `cargo update -p h2` clearing RUSTSEC-2026-0258.
11. ~~**`mcp-tools` / server-composition — no request timeout and no panic guard.**~~ Fixed in
    `fix(mcp): a request timeout and a panic guard on the only session there is`.
12. ~~**`cost-metering` / price-tables — two pricing authorities.**~~ Fixed in `fix(cost): one
    pricing authority`. Proof: a test pins the runner against `PriceBook` over the three resolutions
    the old string scan could not do.
13. ~~**`repo-manifest-standard` / spec-ships-with-artifact — the manifest names a spec no clone can
    resolve.**~~ Fixed in `docs(manifest): ship the contract the manifest names`. Checked first: no
    such spec existed anywhere, not merely out of reach. Proof: a control naming a missing
    capability, and a `docs:` pointer at a directory that does not exist, each turn the suite red.
14. (now backlog #2 — DB instrumentation and the long lane, `out-of-budget`.)
15. ~~**`data-access` / row-mapping — `parse_enum` silently defaults on mismatch.**~~ Fixed in
    `fix(store): a stored enum outside its vocabulary is surfaced, not coerced`.

### Found and fixed while draining, not on the backlog

- **`cargo test --workspace` had been RED since 2026-08-08.** Six OTLP tests pinned their fixture to
  the calendar constant 2026-08-01T10:00Z, and ingest refuses a `ts` more than 7 days old, so a
  blocking required check had been failing for over two weeks on a tree nobody had touched. The
  fixture is now anchored to the run (resolved once per process, so the "start time became the event
  timestamp" assertion still holds). Fixed inside the job-coordination commit because it blocked
  verifying anything else.
- **`.ai/manifest.yaml` forbade its own remediation path.** `neverTouch: [target/, Cargo.lock]` read
  as "never change the lockfile", which forbids the `cargo update -p <crate>` that answers a RUSTSEC
  advisory. Split into `neverTouch` (build output) and `generatedNotHandEdited` (changed only by its
  generator), and the distinction is written into the shipped spec §6.
- **The hooks and the gate script shipped 100644.** Committed from a `core.filemode=false` checkout,
  so a POSIX clone would receive a non-executable pre-commit hook — and git silently declines to run
  a hook it cannot execute, which is the quietest possible way for a rung to stop existing.
