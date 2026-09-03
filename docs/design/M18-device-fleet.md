# M18 — Device fleet: enrolled devices, advertised capabilities, capability-routed leases

Size XL · gate contract · wave C · contexts: relay-queue, device-agent, api-server, mcp-server,
store-sqlite-eval, store-postgres · depends on M7 (fenced lease, schedule sweep)

## Problem
The relay is hard-wired to one anonymous device: a single shared `LIGHTTRACK_RELAY_DEVICE_KEY`
(`crates/api/src/relay.rs` ~30-40), a client-asserted `device` name defaulting to `"default"`
(~172-173) that the cloud trusts as identity, and a lease that hands any due task to whoever asks
(`sqlite/relay.rs` ~114-117) — even a device whose action library lacks the action, which then burns
a real attempt on "no action" (`agent/exec.rs` ~51-56). Enqueue validates only that `action_type`
is non-empty (~65-67), so a typo'd action type is indistinguishable from a healthy backlog until it
dead-letters hours later. There is no device table, no heartbeat, no "which devices exist", and zero
relay tools on MCP. `docs/RELAY.md` calls multi-device "future work".

## Design
1. `crates/core/src/device.rs`: `Device { id, project_id: Option<String> /* None = operator-wide */, name, key_prefix, key_hash, capabilities: Vec<String> /* action types or "ns/*" prefixes */, last_seen_at, agent_version, created_at, revoked }`;
   `RelayAdmission { Queued { eligible_devices: u32 } | Refused { reason } }` (closed vocabulary).
2. Store: `create_device`, `list_devices`, `find_device_by_key_prefix`, `touch_device(id, capabilities, version)`,
   `revoke_device`, `count_eligible_devices(action_type)`; change `lease_relay_tasks(device_id, capabilities, lease_secs, max)`
   to filter `action_type IN (…) OR action_type LIKE 'ns/%'`. SQLite + PG (tables `devices`; a
   `devices` block at the END of each DDL; `relay_tasks.device` becomes the device id — keep the
   column, change what is written). Firestore stays `Unsupported` for relay (declared). New
   `Devices` surface in the manifest; conformance section: capability filter routes correctly;
   an unroutable task is never leased.
3. Auth: `ensure_device` resolves `Bearer ltd_<prefix>_<secret>` through the hashed table (reuse
   `auth.rs` key hashing); keep `LIGHTTRACK_RELAY_DEVICE_KEY` as a legacy single-device fallback
   (all capabilities) for one release, logged at startup as deprecated.
4. API: `POST /v1/relay/devices` (admin; secret returned once — HTTP only, never MCP),
   `GET /v1/relay/devices` (admin; liveness = `last_seen_at`), `DELETE /v1/relay/devices/:id`;
   enqueue computes and returns the admission verdict (`queued { eligible_devices }` or 422
   `refused: no enrolled device advertises 'xprice/foo'`); `LeaseReq` gains `capabilities`,
   `agent_version`; drop the client-asserted `device` (ignored if sent). Every route into
   `ROUTE_SCOPES`.
5. Agent: `actions::inventory(actions_dir)` enumerates `<ns>/<name>` dirs with a `prompt.md`;
   `cloud.rs::lease` sends it plus the agent version; `run.rs` prints the inventory at startup;
   config gains `device_key` (replacing the shared env key).
6. Alerts: `relay_task_unroutable` fired from the schedule sweep (M7) when a queued task has had no
   eligible device for longer than a threshold; goes through the existing `Alerter` (M3 may be
   landing concurrently — call the existing `notify_*` shape, do not restructure `alerts.rs`).
7. MCP: read-only `list_relay_tasks`, `get_relay_task`, `list_relay_devices` (targets/secrets
   redacted; `readOnlyHint`). Writes stay behind `LIGHTTRACK_MCP_ALLOW_WRITES`; never mint device
   keys over MCP. CLI `lt relay devices …`. SDKs: surface the admission verdict (`RelayError` on
   `refused`) in Python/TS.
8. `docs/RELAY.md`: enrolment, capabilities, admission verdicts; retire "future work".

## Out of scope
Pricing relay runs / enqueue admission against limits (M5). Relay events becoming judgeable (M19).

## Gates
`cargo build/test/clippy` for lighttrack-core, -store, -store-pg, -api, -agent, -mcp, -cli;
SQLite conformance; `tests_relay.rs` extended (enrol → lease filtered by capability → unroutable
enqueue 422); SDK tests for the new error kind.

## Evaluation
Before: 1 shared device key; `device` trusted verbatim; enqueue validation = non-empty; 0 device
endpoints; 0 relay MCP tools; missing action burns attempts for hours. After: hashed per-device
keys; leases filtered by advertised capabilities; enqueue returns `queued{eligible_devices}` or
`refused`; `GET /v1/relay/devices` shows liveness; 3 read-only MCP tools.
