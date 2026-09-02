# M1 — Capability manifest on the `Store` trait

Size L · gate contract · wave A · contexts: store-interface, store-postgres, store-firestore,
store-sqlite-core, store-sqlite-eval, api-server, mcp-server

## Problem
`Store` has ~94 methods, ~45 of them defaulting to `Err(StoreError::Unsupported(..))`
(`crates/store/src/lib.rs` — prompts ~1105-1131, relay ~1213-1250, collective ~1258-1274,
maintenance ~1285-1301, forecast series ~820-838, margin breakdowns ~1176-1206). `PgStore` (the
production backend on Neon) inherits ~24 of those, `FirestoreStore` ~24. Only two capability flags
exist — `serves_traces()` (~959) and `admission_is_atomic()` (~704) — and nothing outside the store
crates calls either. The conformance suite (`crates/store/src/conformance.rs`) asserts refusal only
for traces (~619-648) and *skips* on `Unsupported` for `cancel_job` (~1514), `renew_job_lease`
(~1656) and the relay section (~1799); it has no coverage for forecast series, margin breakdowns,
`update_project`, prompts, collective, storage. Live consequence: `PUT /v1/projects/:id`
(`crates/api/src/projects.rs` → `update_project`) answers 501 on Postgres, so the redaction policy
cannot be changed in production. Nobody decided that.

## Design
1. `crates/store/src/capabilities.rs` (new, <300 LOC):
   ```rust
   pub enum Surface { EventsCore, EventFilters, Traces, Forecast, MarginBreakdowns, Prompts, Relay,
                      Collective, ProjectAdmin, KeyAdmin, LimitLifecycle, JobLeases, Maintenance, Metrics }
   pub struct Capabilities { pub backend: &'static str, pub surfaces: BTreeSet<Surface>, pub atomic_admission: bool }
   pub const SURFACE_METHODS: &[(Surface, &[&str])]  // every trait method name → exactly one surface
   ```
   Unit test: every `fn` declared in `lib.rs`'s `trait Store` appears in `SURFACE_METHODS` exactly
   once (parse `lib.rs` with `include_str!` at test time; regex on `^    fn ([a-z_0-9]+)\(`).
2. `Store::capabilities(&self) -> Capabilities` — **required, no default**. Implement in
   `SqliteStore` (all surfaces), `PgStore`, `FirestoreStore` (declare exactly what they implement
   today). Re-express `serves_traces()` and `admission_is_atomic()` as default methods reading the
   manifest; keep them for callers.
3. Conformance driver: in `conformance.rs`, replace the per-section "skip on Unsupported" with
   `for surface in Surface::ALL { if caps.has(surface) { run_section } else { assert_all_refuse(surface) } }`.
   `assert_all_refuse` calls every method in the surface with dummy args and asserts
   `Err(Unsupported)`. Add minimal sections for Forecast (`daily_usage`, `daily_cost_by_dimension`),
   MarginBreakdowns (`tokens_by_dimension`, `customer_cost_by_*`), ProjectAdmin (`update_project`),
   Prompts (create/get/list/version), Collective (upsert/list/purge). Split `conformance.rs` if it
   grows: `conformance/{mod,driver,forecast,margin,prompts,collective}.rs`.
4. `docs/PARITY.md` generated: a `#[test]` in `crates/store/tests/parity_doc.rs` renders the
   matrix (surface × backend → full / refused) from the three backends' manifests and fails when
   the checked-in file differs (same pattern as `crates/store/tests/ts_format_guard.rs`). Build the
   Pg/Firestore manifests in the test without a live connection — make `capabilities()` a pure
   function of the type (e.g. `PgStore::CAPABILITIES` const) so the doc test needs no DB.
5. API: `GET /v1/capabilities` (any authenticated principal) → `{backend, surfaces:[..], atomic_admission}`;
   include `capabilities.surfaces` in `/health`. One startup `tracing::warn!` per undeclared surface
   after store construction in `crates/api/src/main.rs` (wiring only — put the logic in a small
   `crates/api/src/capabilities.rs`). Delete the two hand-written `eprintln!` banners in
   `crates/store-firestore/src/lib.rs` (~72-82).
6. MCP: `get_capabilities` read-only tool (`readOnlyHint`) in `crates/mcp/src/read.rs` + schema.
7. **Port `update_project` on Postgres** in the same change (one UPDATE statement,
   `crates/store-pg/src/projects.rs`) and flip `ProjectAdmin` to declared. Do not port anything else.

## Out of scope
Porting other surfaces (M2, M20, M27 do that). Changing `Unsupported → 501` mapping.

## Gates
`cargo build/test/clippy -p lighttrack-store -p lighttrack-store-pg -p lighttrack-store-firestore
-p lighttrack-api -p lighttrack-mcp`; `cargo test -p lighttrack-store --test sqlite_conformance`;
the new parity-doc test; PG/Firestore conformance are env-gated — run the SQLite one and make the
Pg/Firestore manifests honest by reading their `impl Store` blocks.

## Evaluation (to re-measure after build)
Before: 0 trait methods mapped to a surface; refusal asserted for 5 methods; `update_project` 501
on PG. After: 100% mapped (test), every method has a full-or-refusal assertion, `docs/PARITY.md`
generated, `GET /v1/capabilities` live, `PUT /v1/projects/:id` works on PG.
