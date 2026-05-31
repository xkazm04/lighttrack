# LightTrack — Firestore backend

GCP-native `Store` backend, kept per the full-scope decision (Postgres stays the cross-cloud default;
Firestore is for teams all-in on GCP). Status: **implemented** — `crates/store-firestore`
(`FirestoreStore`), selected by `LIGHTTRACK_DATABASE_URL=firestore://<project>`, implements the full
`Store` trait via the REST approach below (modules: `rest`, `codec`, + one per domain). This doc
records the design/rationale.

## Approach: REST over `reqwest` (not gRPC)
Use the **Firestore REST API** (`https://firestore.googleapis.com/v1`) through the existing `reqwest`
client rather than the gRPC `firestore`/`gcloud-sdk` crates. Rationale: gRPC pulls `tonic`+`ring`/`aws-lc`
(NASM/CMake build pain on Windows); REST reuses our proven `reqwest` + `native-tls` (SChannel on Windows).
New crate `crates/store-firestore` implements the same `Store` trait; selected by
`LIGHTTRACK_DATABASE_URL=firestore://<project-id>`.

## Document model
One collection per current table — `events`, `projects`, `api_keys`, `scores`, `benchmarks`,
`benchmark_runs`, `datasets`, `dataset_items`, `rubrics`, `jobs`, `model_prices`. Document id = the
entity id (for `model_prices`, `"<provider>__<model>"`). Keep the **same logical schema**: store
timestamps as **fixed-width `RFC3339(Nanos,Z)` strings** (Firestore `stringValue`) so lexicographic
range filters / ordering match the SQLite + Postgres backends exactly; ints → `integerValue`, floats →
`doubleValue`, JSON blobs (`input`, `metadata`, `dataset`, `report`, …) → `stringValue`.

## The aggregation problem (no GROUP BY / SUM)
Firestore can't aggregate arbitrarily server-side, so two methods aggregate **client-side**:
- `usage_since(project, since)` → `runQuery` `where project_id == X AND ts >= since`, then sum
  cost/calls/tokens in Rust. Needs a composite index `(project_id ASC, ts ASC)`.
- `cost_summary(project)` → query the project's events, group+sum by provider/model in Rust.

These are O(matched-docs) reads — fine at the target load (≤1k calls/hr). At scale, maintain rollup
counter docs (incremented on ingest) or push events to the analytical `EventSink` (BigQuery/ClickHouse).
All `list_*` / `get_*` map to `runQuery` (`orderBy` + `limit`) / document GET.

## Atomic job claim
`claim_job` uses a Firestore **transaction**: `beginTransaction` → read the oldest `queued` (or stale
`running`) job → `commit` an update to `status=running` with the read precondition. No `SKIP LOCKED`, but
the transaction's optimistic concurrency (+ bounded retry) gives the same single-claim guarantee across
parallel `lt-runner serve` workers.

## Auth
- **Local / CI:** the **Firestore emulator** (`FIRESTORE_EMULATOR_HOST`) needs no token — REST calls go
  straight to the emulator. This is how we verify (no GCP account required).
- **Cloud:** an OAuth2 bearer token. v1 supports the **metadata-server token** (when running on GCP —
  Cloud Run/GCE/GKE workload identity). A service-account-JSON path (sign an RS256 JWT → exchange for a
  token) is a follow-up; until then, run the Firestore-backed API on GCP compute (ADC) or use the emulator.

## Crate structure (per CLAUDE.md: ≤300 LOC/file, per-domain modules)
```
crates/store-firestore/
  Cargo.toml                # reqwest (native-tls), serde, serde_json, lighttrack-core + -store
  src/lib.rs                # FirestoreStore + connect() + `impl Store` delegating to modules
  src/rest.rs               # REST client (GET/PATCH/runQuery/commit) + typed-value encode/decode
  src/events.rs src/projects.rs src/scores.rs src/prices.rs src/limits.rs
  src/benchmarks.rs src/datasets.rs src/rubrics.rs src/jobs.rs
```

## Verification plan
Run the emulator in Docker (e.g. `google/cloud-sdk` `gcloud emulators firestore start`, or a community
`firestore-emulator` image), set `FIRESTORE_EMULATOR_HOST=localhost:8080`, start the API with
`LIGHTTRACK_DATABASE_URL=firestore://demo`, and smoke-test the same flows used for Postgres (ingest →
cost → limit trip → score; then benchmarks/jobs). Add a gated unit test keyed on `FIRESTORE_EMULATOR_HOST`.

## Implementation plan
- **Part 1 (core):** `rest.rs` (value codec + http) + `connect` (emulator/ADC) + events / projects /
  api_keys / prices / scores / limits, with client-side aggregation for `cost_summary` / `usage_since`.
  Verify against the emulator. Wire the `firestore://` branch into the API store-selection block.
- **Part 2:** benchmarks, benchmark_runs, datasets, dataset_items, rubrics, jobs (+ transactional
  `claim_job`). Then a composite-index note for deployment.
