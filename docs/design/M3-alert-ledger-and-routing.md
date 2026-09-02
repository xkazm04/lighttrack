# M3 — Persisted alert ledger with per-project routing and signed, vetted delivery

Size XL · gate contract · wave C · contexts: alert-delivery, api-server, alert-responder,
predictive-forecast, limit-enforcement, store-* · subsumes the architect item "decouple alerts.rs
from concrete SqliteStore"

## Problem
Every alert (breach, warning, forecast, error-spike, score-drop, relay-dead, bench-run) is fired
from process memory and forgotten. `Alerter.last_sent`/`error_windows`/`score_windows` are
`Mutex<HashMap<..>>` (`crates/api/src/alerts.rs` ~103-105); `docs/ALERTS.md` ~117 admits dedup
resets on restart and each replica alerts independently — production is multi-instance Cloud Run.
Delivery outcomes are only `tracing::warn!` (`alerts/channels.rs` ~237-278). Attribution opens a
second SQLite handle from a file path and is `None` on PG/Firestore (`alerts.rs` ~54-57, ~497-499).
Routing is one env-global channel set (`alerts.rs` ~10-23, ~113-142; `ALERTS.md` ~115 "per-project
routing would need a schema/Store change — deferred"). `post_webhook` (~228-243) posts an unsigned
body to any URL with no scheme check, no private-range refusal, default redirects, no body cap. The
responder answers 200 even when it drops the payload (`responder/webhook.rs` ~75-86), writes its
report to local disk (`report.rs` ~43-63) and keeps its breaker state in memory (`breaker.rs`).

## Design
1. `crates/core/src/alert.rs`: `Alert { id, project_id: Option<String>, kind: AlertKind, dedup_key, severity, payload: Value, fired_at, delivered: Vec<Delivery { channel_id, ok, status, at }>, acked_at, acked_by, resolution: Option<Value> }`;
   `AlertKind` enum over the seven existing kinds; `AlertChannel { id, project_id: Option<String> /* None = global */, kind: Webhook|Ntfy|Email, target, secret_hash: Option<String>, min_severity, kinds: Vec<AlertKind>, enabled, created_at }`.
2. Store (all three backends, parity mandatory — this is the product's own audit trail):
   `insert_alert_dedup(&Alert, cooldown) -> Admitted | Suppressed` as ONE atomic store step
   (SQLite tx; PG `INSERT … ON CONFLICT (dedup_key) WHERE fired_at > now - cooldown DO NOTHING` or
   advisory lock; Firestore transaction on a `dedup/<key>` doc), `mark_delivery`,
   `list_alerts(filter { project, kind, since, acked }, cursor)`, `ack_alert`,
   `attach_alert_resolution`, `create/list/delete_alert_channel`, `channels_for(project)`.
   Tables `alerts`, `alert_channels` (SQLite/PG self-contained blocks at the END of each DDL;
   Firestore collections). New `Alerts` + `AlertRouting` surfaces in the manifest; conformance
   sections: dedup across two `Alerter`s over one store yields one admitted; channel round-trip.
3. `Alerter`: `should_send_key` → store `insert_alert_dedup` (in-memory map stays as a
   write-through cache); write the row **before** spawning delivery; `channels::deliver_*` call
   `mark_delivery`. Error/score windows may stay in memory (they are rate detectors, not facts)
   but say so in a comment. `RejectionLedger`: periodic flush of bucket deltas as
   `AlertKind::IngestRejected` rows (never as events). Attribution takes `&dyn Store` from
   `AppState` (`attribution::fetch` already accepts it); delete `attribution_db_from_env` and the
   backend-selection duplicate at `alerts.rs` ~496-501.
4. Routing: `channels_for(project) = project channels ∪ global channels`; env config becomes the
   implicit global rows (synthesised at startup, not persisted) so existing deployments are
   unchanged. Dedup key extended with channel id. `PUT/GET/DELETE /v1/projects/:id/alert-channels`
   (admin), `POST /v1/alert-channels/:id/test`. Secrets: returned once on create (mirror the API
   key show-once pattern); support current+previous for rotation.
5. Delivery security (`channels/sign.rs`, `channels/vet.rs`): webhook posts carry
   `X-LightTrack-Signature: t=<unix>,v1=<hex hmac-sha256 over "t.body">`; destinations must be
   `https://` (loopback allowed in dev mode), resolved and refused if private/link-local at
   configure AND delivery time; redirects `Policy::none()`; response body capped. `hmac`/`sha2`
   are already in tree via `crates/billing`.
6. Routes: `GET /v1/alerts?project=&kind=&since=&acked=&cursor=` (READ), `POST /v1/alerts/:id/ack`
   (MANAGE/admin), `POST /v1/alerts/:id/resolution` (admin or responder key). Add every route to
   `ROUTE_SCOPES`. MCP: `list_alerts` (read), `ack_alert` (write-gated). CLI `lt alerts …`.
   Render: alerts table.
7. Responder: read `alert_id` from the webhook payload (additive field); POST the diagnosis
   (report path, cost, ok, act outcome) as the alert's resolution; derive breaker cooldown counts
   from the ledger via `GET /v1/alerts?kind=…&since=` instead of memory. Responder verifies the
   webhook signature when `LIGHTTRACK_RESPONDER_WEBHOOK_SECRET` is set. Fix the incidental defect:
   `responder/enrich.rs` sends no bearer token — add `LIGHTTRACK_API_KEY` to its config.
8. `crates/api/src/alerts.rs` is large: split `alerts/{mod,ledger,routing,channels,sign,vet,attribution}.rs`,
   each ≤300 LOC. `lock().unwrap()` on alert mutexes → `unwrap_or_else(|p| p.into_inner())`.
9. `docs/ALERTS.md`: dedup is durable; multi-replica caveat removed; signature contract and
   destination rules documented.

## Out of scope
Threshold/escalation logic (M4, merged). Forecast gating (M27).

## Gates
`cargo build/test/clippy` for lighttrack-core, -store, -store-pg, -store-firestore, -api,
-responder, -mcp, -cli, -render; SQLite conformance incl. the two new sections; a signature
round-trip test (sign → verify) and a vet test (private IP refused, http refused, redirect not followed).

## Evaluation
Before: 3 in-memory maps + 1 in-memory ledger; 0 endpoints listing alerts; 0 delivery outcomes
recorded; attribution `None` on 2/3 backends; 0 signature headers; 0 destination checks. After:
`GET /v1/alerts` with `delivered`/`acked_at`/`resolution`; two `Alerter`s over one store → 1
delivery (conformance); 100% of webhook posts signed; refusal count for bad destinations.
