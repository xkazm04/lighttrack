# Performance Optimizer — Projects & Access Control

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. API-key verification hits the store on every authenticated request (no cache)
- **Severity**: High
- **Category**: auth-path / missing-cache
- **File**: `crates/store/src/sqlite/projects.rs:73-80`, `crates/store-pg/src/projects.rs:68-78`, `crates/store-firestore/src/projects.rs:44-48`
- **Scenario**: Enforced mode, steady ingest. Every protected route (`authenticate` in `guards.rs:39-52`) resolves a project key by calling `find_api_key_by_prefix`. A single busy SDK client sending 50–500 events/s produces one prefix lookup per request. The same handful of keys is re-fetched thousands of times/minute.
- **Root cause**: `find_key_by_prefix` runs a fresh DB round-trip for each request. `idx_api_keys_prefix` exists, so the lookup is index-served — but the round-trip itself is the cost. On SQLite the read is taken through the single global `Mutex<Connection>` (`sqlite/mod.rs:84`), so every auth lookup *serializes against all other DB work* (ingest inserts, reads, the detached touch write) on one lock. On Firestore, `find_api_key_by_prefix` is a `runQuery` — a **billed document read per authenticated request**, i.e. a metered charge scaling linearly with total API traffic, not with the number of keys.
- **Impact**: SQLite: auth adds lock-hold time to the busiest path, capping effective concurrency at the mutex. Firestore: ~1 billed read × total request volume (e.g. 100 req/s ≈ 8.6M billed reads/day) purely to re-verify a static set of keys. A cache collapses this to ~O(number of distinct keys) refills per TTL.
- **Fix sketch**: Add a small in-memory TTL cache in the `api` auth layer keyed by `prefix → {key_hash, revoked, project_id}` (e.g. `moka`/`DashMap` + 30–60s TTL, bounded to a few thousand entries). On hit, run `verify_key` against the cached hash and skip the store entirely; on miss, fetch and populate. Hashing (SHA-256) stays per-request — that is the security-correct compare and is cheap; only the store round-trip is cached.
- **Trade-offs**: Revocation latency — a revoked key stays honored until its cache entry expires (bounded by the TTL). Mitigate with a short TTL and/or an explicit cache-invalidation hook on the revoke path. This is the standard revocation-vs-throughput trade; a 30–60s window is normally acceptable for API keys.

## 2. `touch_api_key` performs a write on every authenticated request
- **Severity**: High
- **Category**: auth-path / hot-path
- **File**: `crates/store-firestore/src/projects.rs:50-54`, `crates/store/src/sqlite/projects.rs:82-88`, `crates/store-pg/src/projects.rs:80-88`
- **Scenario**: Same steady-ingest path. After a successful verify, `guards.rs:47-51` fires a detached `touch_api_key(id, now())` to stamp `last_used_at` — once **per authenticated request**.
- **Root cause**: An unconditional `UPDATE api_keys SET last_used_at = ?` (SQLite/PG) or `patch_fields` (Firestore) per request. Firestore: a **billed document write per request** — writes are pricier than reads and this doubles the per-request Firestore cost from finding #1 (one read + one write each). SQLite: the write takes the same global `Mutex<Connection>`, so a fire-and-forget "best effort" stamp actually contends for the one lock the ingest path needs, and each write dirties a page / forces WAL churn.
- **Impact**: On Firestore, `last_used_at` at second granularity costs one write per request forever — e.g. 100 req/s ≈ 8.6M billed writes/day for a field almost no one reads at that resolution. On SQLite it converts every read-only auth into a lock-acquiring writer.
- **Fix sketch**: Coalesce the stamp. Keep an in-process `prefix → last_touched_at` map and only issue `touch_api_key` when the stored value is older than a threshold (e.g. 5–15 min); skip otherwise. This drops touch volume by ~100–1000× while keeping `last_used_at` usefully fresh. Pairs naturally with the cache in #1 (cache entry can hold the last-touched timestamp).
- **Trade-offs**: `last_used_at` becomes coarse (accurate to the threshold), which is fine for its purpose (spotting stale/unused keys). None material otherwise — the write is already best-effort/detached.

## 3. Firestore key lookup is a collection query, not a direct document get
- **Severity**: Medium
- **Category**: auth-path / n-plus-one (per-request round-trip shape)
- **File**: `crates/store-firestore/src/projects.rs:31-48`
- **Scenario**: Firestore backend under enforced auth. Even with the cache in #1, every cache-miss (cold start, new key, TTL expiry) resolves a key via `find_api_key_by_prefix` → `rest.query("api_keys", [prefix EQUAL], limit 1)`.
- **Root cause**: Keys are stored under `create_api_key` keyed by the random `k.id` (`put_doc("api_keys", &k.id, …)`, line 41), so lookup-by-prefix *must* be a `runQuery` (structured query, requires the prefix single-field index, and is the query-priced path) rather than a direct `get_doc(collection, id)` — the cheapest, index-free Firestore read. The lookup key (prefix) and the document id (uuid) are different, forcing the query shape.
- **Impact**: Every miss pays query-path cost/latency instead of a point read. Bounded because #1's cache absorbs most traffic, but it makes cold auth and cache refills more expensive than necessary, and query round-trips have higher tail latency than point gets.
- **Fix sketch**: Mirror the key under a prefix-addressable document id (e.g. `put_doc("api_keys_by_prefix", &k.prefix, {id})` alongside the canonical doc, or key the doc by prefix directly since prefixes are unique-enough and already the lookup key). Then `find_api_key_by_prefix` becomes a single `get_doc` by prefix — a point read, no query index needed. Write path adds one extra `put_doc` at key creation (rare).
- **Trade-offs**: A second document to keep consistent at key creation/revocation (create/revoke are infrequent admin ops, so the extra write is negligible). Prefix-collision would need handling if prefixes are ever non-unique; today they are 8 hex chars from a UUID and effectively unique, but keying by full id-in-a-map avoids even that. SQLite/PG already index the prefix column, so this finding is Firestore-specific.
