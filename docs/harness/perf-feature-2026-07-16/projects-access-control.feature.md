# Feature Scout — Projects & Access Control

> Total: 3
> Critical: 1 | High: 1 | Medium: 1 | Low: 0

## 1. Per-project redaction policy is decorative — stored, displayed, never enforced

- **Severity**: Critical
- **Category**: half-implemented
- **File**: `crates/core/src/project.rs:4-14`, `crates/api/src/projects.rs:18-42`, `crates/render/src/projects.rs:16-26`
- **Scenario**: An operator creates a project with `{"name":"prod","redaction":"drop"}` (the documented value — the MCP tool schema at `crates/mcp/src/write.rs:46` advertises `enum: ["none","hash","drop"]` as "payload persistence"). `GET /v1/projects` echoes `redaction: drop`, and the operator table renders a **Redaction** column reading `drop`. They conclude prompts/outputs are not persisted for that project. They are persisted, verbatim. The team ships customer PII to a project they believe is configured not to keep it.
- **Root cause**: `Redaction` is written by all three backends (`store/src/sqlite/projects.rs:16`, `store-pg/src/projects.rs:21`, `store-firestore/src/projects.rs:17`) and read back into `Project` — but **no consumer of `Project::redaction` exists anywhere in the workspace**. A full-crate grep for `redaction` finds only writes, reads-into-the-struct, and the render column; the ingest path never calls `get_project`. The actual redaction control is `crates/api/src/redact.rs`, which is env-only (`LIGHTTRACK_REDACT_INGEST=off|all|p1,p2`) and — its own header comment says so — deliberately routes per-project "without a schema/`Store` change". So two parallel policy systems exist: one real and invisible (env), one visible and inert (the project field). Worse, `Redaction::Hash` and `Redaction::Drop` have **no implementation at all**: `redact.rs` only does regex PII scrubbing (`scrub_value`), so even wiring the field up gives you nothing for two of three variants.
- **Impact**: This is the privacy/compliance surface of the product and it currently makes a false claim in the UI. Redaction/masking policy is a procurement checklist item (Langfuse and Helicone both sell per-project masking); a control that reports `drop` while storing raw payloads is worse than having no control, because it converts an honest gap into a silent data-retention incident. Fixing it also retires the undiscoverable env var.
- **Fix sketch**:
  1. Implement the missing variants in `redact.rs` next to `redact_event`: `Hash` → replace `ev.input`/`ev.output` with `json!({"sha256": hex})` (preserves presence/diff, which is what the doc comment on `project.rs:10` promises); `Drop` → set both to `None`.
  2. Give `AppState` a small `RwLock<HashMap<String, Redaction>>` policy cache, warmed from `list_projects()` at startup and refreshed on create/update (see finding 3). Avoids a `get_project` hit per ingested event — the reason the env shortcut was taken.
  3. In the ingest paths (`events.rs`, `events_batch.rs`), resolve policy per `ev.project_id`: project field if present, else the env `Redactor`. Apply `Drop`/`Hash` before the existing PII scrub.
  4. Keep `LIGHTTRACK_REDACT_INGEST` as a global *floor* (strictest-wins), documented as legacy, so existing deploys don't silently weaken.
  5. Until 1–4 land, the honest interim is one line: drop the **Redaction** column from `render/src/projects.rs` and the field from the create API, so nothing claims what nothing enforces.
- **Trade-offs**: Strictest-wins can surprise an operator who sets `none` on a project while the env says `all` — surface the effective policy in the startup banner (`Redactor::describe` already exists) and in the projects table. The policy cache is eventually-consistent across replicas; bounded by refresh interval, acceptable for a retention default but worth noting in docs.

## 2. API keys can be minted but never listed, rotated, or revoked

- **Severity**: High
- **Category**: enterprise-readiness
- **File**: `crates/api/src/projects.rs:75-114`, `crates/core/src/project.rs:38-53`, `crates/store/src/lib.rs:409-414`
- **Scenario**: A contractor commits `lt_ab12cd_…` to a public repo. The operator opens LightTrack to kill the key and finds: no endpoint lists the project's keys, no endpoint revokes one. The only remedies are `UPDATE api_keys SET revoked = 1` by hand against SQLite/Postgres/Firestore, or rotating the whole deployment. Separately, an operator wanting to prune stale keys can't see which are stale — even though the server has recorded exactly that for every request.
- **Root cause**: The data model is complete but the surface is not. `ApiKey.revoked` is honored on the hot path (`guards.rs:43` — `if !k.revoked && auth::verify_key(...)`) and `last_used_at` is written on **every authenticated request** (`guards.rs:47-51`, backed by `touch_api_key` in all three stores), yet nothing can ever set `revoked = true` and nothing can ever read `last_used_at` back. The route table exposes exactly one key route (`main.rs:292`: `POST /v1/projects/:id/keys`); the `Store` trait offers only `create_api_key` / `find_api_key_by_prefix` / `touch_api_key` — no list, no update. So `revoked` is an enforced-but-unreachable flag, and `last_used_at` is a write-only column: the storage cost of an audit trail with none of the value. There is also no `expires_at` on the model at all.
- **Impact**: Key lifecycle — rotate, revoke, expire, last-used visibility — is the standard enterprise access-control checklist, and every named competitor ships it. Leak response is currently a DBA task, which for a self-hosted observability product means the answer to "we leaked a key" is "SSH in". Surfacing `last_used_at` is nearly free (the data is already there) and directly enables the two workflows operators actually ask for: overlapping-key rotation, and pruning keys nobody uses.
- **Fix sketch**:
  1. `Store` trait: add `list_api_keys(&self, project: &str) -> Result<Vec<ApiKey>>` and `revoke_api_key(&self, id: &str) -> Result<()>`. Give both a default impl (`Ok(vec![])` / `Ok(())`) so unported backends compile — matching the `get_limit_rule` precedent at `store/src/lib.rs:421`.
  2. Implement in all three: SQLite `SELECT … WHERE project_id = ?1` + `UPDATE api_keys SET revoked = 1 WHERE id = ?1`; Postgres mirrors it; Firestore uses `rest.query("api_keys", [("project_id","EQUAL",…)])` + `patch_fields(…, &["revoked"])`, both of which already exist in the file.
  3. `GET /v1/projects/:id/keys` → `[{id, name, prefix, created_at, last_used_at, revoked}]`. **Never** return `key_hash` — build a `KeyInfoResp` rather than serializing `ApiKey`, which currently derives `Serialize` over the hash.
  4. `DELETE /v1/projects/:id/keys/:kid` → revoke (soft; keep the row for audit). Admin-gated via the existing `ensure_can_admin`.
  5. Rotation falls out of 3+4: mint a new key, migrate callers using `last_used_at` on the old one to confirm it's drained, revoke.
  6. Follow-up (own change): add `expires_at: Option<DateTime<Utc>>` to `ApiKey` + a `!expired` check alongside the `revoked` check in `guards.rs:43`.
- **Trade-offs**: Revocation latency depends on any future key cache (none today — `guards.rs` hits the store per request, so revocation is immediate; keep that property in mind if a cache is added for the finding-1 policy work). Adding `expires_at` touches three schemas, hence splitting it out.

## 3. Projects are immutable after creation — `enabled` is unsettable and unenforced

- **Severity**: Medium
- **Category**: capability-gap
- **File**: `crates/api/src/projects.rs:31-37`, `crates/core/src/project.rs:27-28`, `crates/render/src/projects.rs:21-24`
- **Scenario**: A project is created as `staging-tmp` with the wrong redaction setting. There is no way to rename it, no way to change the policy, and no way to turn it off — the operator's only recourse is to create a second project and abandon the first, which permanently splits that app's cost/trace history across two ids. Meanwhile a runaway staging deploy floods ingest and the operator has no switch to close the door short of revoking every key.
- **Root cause**: The `Store` trait has `create_project` / `get_project` / `list_projects` and no update (`store/src/lib.rs:405-407`); `main.rs:291` routes only `POST` and `GET` on `/v1/projects`. `Project.enabled` is hardcoded `true` at `projects.rs:34` and is never read outside tests — the render layer draws a ✅/— column (`render/src/projects.rs:21-24`) for a flag that can only ever be `true`, and the ingest path never consults it, so even a hand-flipped `enabled = 0` in the DB would still accept events. Like `redaction`, it's a promised control with no consumer.
- **Impact**: An always-on tri-state (name/enabled/redaction) that operators can't touch makes projects one-shot — every misconfiguration becomes a permanent id and fragmented history. `enabled` is the natural kill switch for a noisy tenant and the natural way to retire an app without deleting its history; it's also the cheap half of the finding-1 fix, since both need the same write path. Wiring it costs one guard on ingest.
- **Fix sketch**:
  1. `Store`: add `update_project(&self, p: &Project) -> Result<()>` (replace mutable fields: name/enabled/redaction), default-impl'd for unported backends. SQLite/PG are one `UPDATE`; Firestore is `patch_fields("projects", id, …, &["name","enabled","redaction"])`.
  2. `PATCH /v1/projects/:id` with `{name?, enabled?, redaction?}` — admin-gated, read-modify-write via the existing `get_project`, 404 on missing (mirror the check at `projects.rs:85-87`).
  3. Enforce `enabled` in the ingest path: reject with 403 `"project disabled"` when false, reusing the policy cache from finding 1 so it costs no extra query. Do this in the same change as the PATCH — an enforceable-but-unsettable flag and a settable-but-unenforced flag are both bugs.
  4. Then the ✅/— column in `render/src/projects.rs` becomes truthful.
- **Trade-offs**: Disabling a project silently drops a client's telemetry — return a distinct 403 body so SDKs can log a clear cause rather than retrying blind. Deliberately *not* proposing project deletion: events/traces/datasets/prompts all key off `project_id` with no cascade, so deletion is a much larger integrity question; `enabled = false` is the right retire-without-orphaning primitive.
