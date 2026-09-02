# M17 — Tenant scope as a typed parameter on every `Store` read: D13 applied to the whole trait

Size L · gate contract · wave E · contexts: store-interface, store-sqlite-*, store-postgres,
store-firestore, api-server · runs after every handler-adding wave (A–D) so it sees all reads

## Problem
D13 fixed cross-tenant reads for traces by putting the project filter in the query (404, never 403),
and the conformance suite pins it — for traces only. Many point reads still fetch by bare id and
authorise afterwards: `get_event`, `get_benchmark`, `list_benchmark_runs`, `get_dataset`,
`list_dataset_items`, `get_rubric`, `get_job`/`list_jobs`, `get_limit_rule`/`update_limit_rule`/
`delete_limit_rule`, `scored_event_ids`, `get_prompt_by_id`, `get_relay_task` — plus whatever the
waves added (schedules, devices, alerts, labels, calibrations, margin policies, price history).
Handlers compensate with `forbidden(...)` when `project_id` mismatches (`benchmarks.rs`,
`datasets.rs`, `rubrics.rs`) — a 403 that confirms the id exists, the oracle D13 removed. `jobs`
has no `project_id` column at all; a project key can `GET /v1/jobs` and read every project's
payloads.

## Design
1. `crates/store/src/scope.rs`: `pub enum Scope<'a> { Project(&'a str), Operator }` with
   `From<Option<&str>>` for the migration window and `Scope::sql_pred(col) -> (String, Vec<param>)`
   mirroring `project_pred` so the predicate stays sargable.
2. Enumerate every read/update/delete on `trait Store` whose row carries a `project_id` and takes no
   scope (write the list in the PR description; the M1 `SURFACE_METHODS` table is the checklist —
   walk every surface). Change `project: Option<&str>` params to `Scope` and **add** `scope: Scope`
   to the unscoped ones. `list_prices`/`upsert_price`/price history stay operator-global by design
   (doc comment says so). A pure `#[test]` parses `lib.rs` and asserts no remaining
   `project: Option<&str>` parameter and that every method in a project-bearing surface has a
   `Scope` parameter (allowlist the global ones).
3. Backends: SQLite/Postgres add `AND project_id = ?` under `Scope::Project` in every per-domain
   module; Firestore adds an `EQUAL` filter (`project_filter` pattern). Full parity — refusal is not
   acceptable for point reads.
4. Schema: `jobs.project_id TEXT` nullable (`ADDED_COLUMNS_LATE` + PG `ALTER … IF NOT EXISTS` tail
   block; Firestore field); `Job += project_id: Option<String>`; `enqueue` stamps it from the
   benchmark/schedule. `NULL` = operator/legacy — operator scope sees them, a project scope does not.
5. Conformance: a generic `tenancy(store)` section — for every project-bearing entity type create one
   under `pid` and a twin under `other`; assert `Scope::Project(pid)` sees exactly its own,
   `Scope::Project(third)` sees `None`/empty, `Scope::Operator` sees both. Generalise the existing
   trace collision test into it.
6. API: every handler passes the principal's scope (`Principal::Project → Scope::Project`,
   admin/dev → `Scope::Operator`); delete the post-hoc `forbidden` branches; a foreign id is 404
   everywhere. `list_jobs`/`get_job` scoped. Document the 403→404 change in ARCHITECTURE §9 and a
   D13 addendum in DECISIONS.md. Check `clients/*` for code branching on `forbidden` vs `not_found`.
7. MCP: no code change; tool descriptions mention 404 semantics.

## Out of scope
New reads. Schema generation (M14).

## Gates
`cargo build/test/clippy` for lighttrack-core, -store, -store-pg, -store-firestore, -api, -runner,
-mcp, -cli; SQLite conformance incl. the tenancy section; the trait-signature test.

## Evaluation
Before: ≥17 unscoped reads; 3+ handlers authorise post-hoc with 403; `jobs` has no project_id; 1
entity type collision-tested. After: 0 unscoped project-bearing reads (test); 0 post-hoc 403s; all
entity types collision-tested; project key cannot list foreign jobs (test).
