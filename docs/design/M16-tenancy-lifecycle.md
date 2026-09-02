# M16 — Tenancy lifecycle: enforce `enabled`, scope and expire API keys

Size L · gate contract · wave A · contexts: project-management, api-server, event-ingest

## Problem
`Project.enabled` (`crates/core/src/project.rs` ~22-23) is writable via `PUT /v1/projects/:id`
(`crates/api/src/projects.rs` ~155-157) and read by exactly one caller, the forecast sweep
(`crates/api/src/forecast_sweep.rs` ~127). `resolve_principal` (`crates/api/src/guards.rs` ~66-91)
checks `revoked` and the hash, never the project and never an expiry; `resolve_ingest_project`
(~113-136) returns the key's project with no lookup. So "disable this project" does nothing.
`Principal::Project { project_id, key_id }` (`crates/api/src/auth.rs` ~85-96) is the only shape and
`resolve_read_project` (~195-212) grants it full read — an ingest key embedded in a client app can
read every stored prompt and output. `ApiKey` (`project.rs` ~39-53) has no `scopes`, no
`expires_at`, no rotation. `DELETE /v1/projects/:id` semantics: check what exists; archive, never delete.

## Design
1. `crates/core/src/project.rs`: `pub enum Scope { Ingest, Read, Manage }` (serde lowercase);
   `ApiKey += scopes: Vec<Scope>` (serde default = `[Ingest, Read]` — the permissive back-compat
   default for one release, logged at key use; documented next default `[Ingest]`),
   `expires_at: Option<DateTime<Utc>>`; `Project += archived_at: Option<DateTime<Utc>>`.
2. Store: `create_api_key` persists `scopes` (JSON text column `scopes`, additive migration on
   SQLite via `ADDED_COLUMNS` in `crates/store/src/sqlite/schema.rs`, Postgres `ALTER TABLE …
   ADD COLUMN IF NOT EXISTS` in `schema/postgres/001_init.sql`, Firestore field) and `expires_at`
   (fixed-width RFC3339 text). Rows without the column read as the permissive default (backfill
   sentinel). `update_project` already exists on SQLite; on Postgres it is being ported by M1 —
   **do not** implement it here; if your branch needs it for a test, gate the test on
   `capabilities()`/`Unsupported` or use SQLite. Add a `rotate_api_key(project, kid, grace)` store
   method or compose it from create + revoke in the handler (prefer composing; no new trait method).
3. `crates/api/src/state.rs`: generalise `redaction_policies: PolicyCache<Redaction>` to
   `PolicyCache<ProjectPolicy { redaction, enabled }>`; `redaction_policy_for` → `project_policy_for`
   (same invalidation on `update_project`).
4. `crates/api/src/guards.rs`: after key verification in `resolve_principal`, load
   `project_policy_for(pid)`; `enabled == false` → **403 `project_disabled`** (new `ErrorCode` in
   `crates/api/src/error.rs`) for ingest and reads by that project's keys; admin principals are
   unaffected. `expires_at < now` → 401 `key_expired`. `Principal::Project += scopes`.
5. `crates/api/src/auth_scopes.rs` (new): `ensure_scope(&Principal, Scope) -> Result<(), ApiError>`
   (admin passes everything) and a route→scope table; a unit test that every `/v1/*` route string in
   `build_router` has a declared scope (read `main.rs` with `include_str!` or keep a parallel list
   compared by string). Apply `ensure_scope` in the handlers: ingest doors (`POST /v1/events`,
   `/v1/events/batch`, OTLP `/v1/traces`, relay settle) = `Ingest`; all GETs under a project key =
   `Read`; limit/prompt/benchmark/dataset/rubric writes under a project key = `Manage`.
6. Handlers: `POST /v1/projects/:id/keys` accepts `scopes`, `expires_at`;
   `POST /v1/projects/:id/keys/:kid/rotate {grace_secs}` mints a successor (same name/scopes,
   secret returned once) and schedules revocation of the old key after `grace_secs` (store
   `expires_at = now + grace` on the old key — no background task needed);
   `DELETE /v1/projects/:id` = archive (`enabled=false`, `archived_at=now`, rows kept).
   `crates/api/src/projects.rs` is ~384 LOC: split `projects_keys.rs` (create/list/revoke/rotate)
   from `projects.rs` (project CRUD/archive).
7. MCP: unchanged (never mints keys). CLI: `lt keys create --scope ingest --expires <rfc3339>`,
   `lt keys rotate`. SDKs: no change (ingest scope suffices).
8. Docs: `docs/ARCHITECTURE.md` §9 (error table gains `project_disabled`, `key_expired`); note that
   this is three capabilities on a key, not RBAC (non-goal stands).

## Out of scope
Scoping every `Store` read by project (M17). Alert routing per project (M3).

## Gates
`cargo build/test/clippy -p lighttrack-core -p lighttrack-store -p lighttrack-store-pg
-p lighttrack-store-firestore -p lighttrack-api -p lighttrack-cli`; SQLite conformance; new
tests: disabled project → 403 on `POST /v1/events`, `/v1/events/batch`, `/v1/traces`; ingest-only
key → 403 on `GET /v1/events`; expired key → 401; route-scope coverage test.

## Evaluation
Before: `enabled` read by 1 non-auth caller; 0 scope/expiry fields; any project key reads all.
After: 4 ingest doors honour `enabled`; every route has a declared scope (test); expiry enforced.
