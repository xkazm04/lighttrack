# Performance Optimizer — Platform Core & API Server

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. Every authenticated request touches the global SQLite mutex twice (uncached read + write-amplified `touch_api_key`)
- **Severity**: High
- **Category**: lock-contention
- **File**: `crates/api/src/guards.rs:39-54`
- **Scenario**: An observability API is read-heavy — dashboards/SDKs poll `GET /v1/events`, `/costs`, `/traces`, `/margin` continuously. With SQLite backing (`crates/store/src/sqlite/mod.rs:83-85` — one process-wide `Mutex<Connection>`), authenticated traffic at even a few hundred req/s serializes here.
- **Root cause**: `authenticate` calls `find_api_key_by_prefix` on the store for every project-keyed request (blocking read under the global mutex). Then, on success, it *unconditionally* fires a detached `tokio::spawn(spawn_blocking(touch_api_key))` — a second store call that takes the **same** global mutex to write a `last_used` timestamp. So each read endpoint request generates one mutex-held SELECT plus one mutex-held UPDATE. The write half is pure amplification: a non-critical "last used" bump contends the single writer lock that every ingest and query also needs, and SQLite serializes writers process-wide.
- **Impact**: Two global-lock acquisitions per authenticated request, one of them a write. At N req/s the connection mutex is the throughput ceiling; the `touch` write roughly doubles lock-hold events and can block real ingest/query work behind a cosmetic timestamp. The read half is also the "uncached round-trip" the parallel access-control audit flagged — confirmed here from the contention angle.
- **Fix sketch**: (a) Cache verified keys in an in-process TTL map (prefix→{key_hash, project_id, revoked}) so the common path skips the DB entirely; invalidate on revoke/create. (b) Throttle `touch_api_key` — coalesce to at most once per key per N minutes (compare against a cached last-touch), or batch timestamps and flush periodically, so reads stop generating writes.
- **Trade-offs**: A key cache means revocation is visible only after TTL (bound it, e.g. 30–60 s). Throttled `touch` makes `last_used` coarse-grained — acceptable for an audit/telemetry field.

## 2. Auth key verification round-trip is on the hot path for the entire API surface
- **Severity**: High
- **Category**: middleware-cost
- **File**: `crates/api/src/guards.rs:22-61`
- **Scenario**: `authenticate` is invoked per-handler (there is no shared auth `Layer`), so every protected route pays its full cost on every call, including bursts of small `POST /v1/events` ingests and tight dashboard polling.
- **Root cause**: For any `lt_`-prefixed token the function always does a DB lookup (`find_api_key_by_prefix`) followed by a SHA-256 verify (`auth::verify_key`). There is no negative cache (unknown/invalid tokens re-hit the DB every time) and no positive cache (valid tokens re-hit every time). Because the lookup is a blocking store call, it also consumes a `spawn_blocking` pool slot per request in addition to the mutex hold in finding #1. Admin-key and dev-no-token paths are cheap; only the project-key path carries this, but that is the production-traffic path.
- **Impact**: A fixed DB+hash tax on essentially 100% of production requests, and an unauthenticated caller spamming random `lt_*` tokens can drive one DB round-trip per request (trivial amplification against the global mutex). This is the dominant per-request platform cost after #1.
- **Fix sketch**: Fold into the same TTL cache proposed in #1 (positive entries), plus a short-lived negative cache for tokens that fail lookup, so repeated bad tokens don't repeatedly hit the store. Optionally hoist auth into a single `middleware::from_fn_with_state` layer so the resolved `Principal` is computed once and shared, rather than re-derived per handler where multiple guard calls occur.
- **Trade-offs**: Caching hashes in memory widens the in-process secret footprint slightly (already holds `admin_key`); TTL bounds staleness. Negative cache must be capacity-bounded to avoid unbounded growth under a token-spray attack.

## 3. `sha256_hex` allocates ~32 heap `String`s per key verification
- **Severity**: Medium
- **Category**: allocation
- **File**: `crates/api/src/auth.rs:50-58`
- **Scenario**: Runs inside `verify_key`, which (per findings #1/#2) executes on every project-keyed request until a cache is added.
- **Root cause**: `finalize().iter().map(|b| format!("{b:02x}")).collect()` invokes the full `format!` formatting machinery and heap-allocates a small `String` for each of the 32 digest bytes, then concatenates them. `hash_with_salt` additionally allocates a `format!("{salt}:{full_key}")` input string each call. So one verify does ~33 short-lived allocations plus formatter overhead for what is fundamentally a fixed 64-char hex encode.
- **Impact**: Tens of allocations per authenticated request — bounded and individually cheap, but pure waste on the hot path and extra allocator pressure under load. Eliminating it is a small, safe constant-factor win (and matters more precisely when request rate is high, i.e. exactly when it's called most).
- **Fix sketch**: Write hex directly into a pre-sized `String` (`let mut s = String::with_capacity(64); for b in digest { write!(s, "{b:02x}"); }`) or use a `hex` encoder; feed the hasher `salt`/`:`/`full_key` via successive `update()` calls to avoid the intermediate concatenation.
- **Trade-offs**: None material — identical output, fewer allocations. (Note: `verify_key`'s `==` on hex strings is not constant-time; that's a security concern, out of scope for this perf pass.)

---

### Checked and deliberately not filed
- **`AppState` clone per request** (`state.rs:17-47`): axum clones state per handler, but every field is an `Arc`/`Copy` — clone is a handful of refcount bumps. Not a bottleneck.
- **`prices: Arc<RwLock<PriceBook>>`** (`state.rs:21`): read-mostly, write only on the rare `PUT /v1/prices`. `std` RwLock readers don't contend meaningfully; no finding (would only matter if a handler `.read().clone()`s the whole book per event — not visible in these files).
- **Error mapping** (`error.rs`): the `serde_json::json!` + `Json` allocation only runs on the error path (cold); `From<StoreError>` is a trivial match. Not hot.
- **Bootstrap** (`main.rs:153-186`): store connect + price seeding already run on a `spawn_blocking` thread and are one-time startup cost, not per-request. Correct as-is.
