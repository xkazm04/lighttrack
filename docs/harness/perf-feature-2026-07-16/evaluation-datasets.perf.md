# Performance Optimizer — Evaluation Datasets

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. Dataset build writes one case per HTTP request, and each triggers a full dataset re-read
- **Severity**: High
- **Category**: per-case-roundtrip
- **File**: `crates/runner/src/dataset.rs:67-89`, `crates/api/src/datasets.rs:86-103`
- **Scenario**: `dataset build --limit 2000` (or an online `schedule` cycle) sampling 2000 events with input into one frozen dataset.
- **Root cause**: The builder loops per event and issues a separate `POST /v1/datasets/{dsid}/items` for each (`dataset.rs:86`). On the server, `add_dataset_item` calls `load_dataset_authorized` on **every** call (`datasets.rs:94`), which runs `get_dataset` — a full dataset fetch — purely to re-check `frozen`/ownership that never change mid-build. So each of the N items costs: 1 HTTP round trip + 1 dataset SELECT/`get_doc` + 1 single-row INSERT/`put_doc`, each its own transaction/request.
- **Impact**: For N=2000: 2000 HTTP round trips, 2000 dataset reads, 2000 write transactions — 3× the necessary store operations. On SQLite the global `Mutex<Connection>` serializes all 4000 statements; on Firestore this is **~2000 billed doc reads + 2000 billed writes**, where the 2000 reads are pure waste (same doc re-read every item). A build that could be one authorization read + a batched write instead pays per case.
- **Fix sketch**: Add a bulk items endpoint (`POST /v1/datasets/{id}/items:batch` taking `Vec<DatasetItem>`) that authorizes/loads the dataset **once**, then inserts in a single SQLite transaction / sqlx multi-row insert / Firestore `batchWrite`. Have `build_from_events` accumulate the scrubbed items and post them in chunks (e.g. 200/req). Independent smaller win: cache the loaded `Dataset` and skip the per-item `get_dataset`.
- **Trade-offs**: Batch insert loses per-item incremental progress printing; keep a per-chunk log line. Larger request bodies — cap chunk size.

## 2. Schedule idempotency check downloads the entire (unbounded, growing) dataset list every cycle
- **Severity**: High
- **Category**: unbounded-result
- **File**: `crates/runner/src/schedule.rs:70-73`, `crates/store-firestore/src/datasets.rs:27-31`, `crates/store/src/sqlite/datasets.rs:31-39`
- **Scenario**: A `schedule --interval 300` daemon running for weeks, producing one dataset per sampled window; the project accumulates thousands of datasets.
- **Root cause**: To decide whether the current watermark was already captured, each cycle calls `GET /v1/projects/{project}/datasets`, which maps to `list_datasets` — a `WHERE project_id = ?` with **no limit** — then does a client-side linear `existing.iter().any(|d| d.name == name)` (`schedule.rs:71`). The cost of checking existence of one name grows with the total number of datasets ever created.
- **Impact**: Cycle cost is O(total datasets), unbounded. On Firestore every cycle bills a full-collection read of all project datasets — after 10k datasets that is **10k billed reads per cycle just to look up one name**, i.e. reads grow quadratically over the daemon's lifetime. On SQLite it deserializes the whole list (each row parses timestamps) under the connection mutex every interval.
- **Fix sketch**: Replace the list-then-scan with an existence lookup by name: a `get_dataset_by_name(project, name)` (indexed `SELECT 1 ... WHERE project_id=? AND name=? LIMIT 1` / Firestore filtered query with `limit 1`). Requires an index on `(project_id, name)`. Falls to O(1) per cycle.
- **Trade-offs**: Needs a new store method + composite index; minor schema/migration work across three backends.

## 3. Loading a dataset's items has no pagination — full in-memory load and full JSON serialization
- **Severity**: Medium
- **Category**: full-load
- **File**: `crates/api/src/datasets.rs:105-115`, `crates/store/src/sqlite/datasets.rs:68-75`, `crates/store-firestore/src/datasets.rs:53-57`, `crates/render/src/datasets.rs:54-86`
- **Scenario**: Loading a large curated dataset (e.g. 20k cases) for a run, or rendering it.
- **Root cause**: `list_dataset_items` runs `SELECT ... WHERE dataset_id=?` (Firestore: filtered query with `None` limit) with no `LIMIT`/cursor, collects every row into a `Vec<DatasetItem>`, and the API returns the whole vec as one `Json` body. Each item also JSON-parses `tags` and `anonymization` per row. `render::items` then materializes all rows again into a table.
- **Impact**: Memory and latency scale linearly and unbounded with dataset size; a single 20k-case fetch allocates the full vec plus 20k `serde_json::from_str` calls for `tags`/`anon`, holds the SQLite mutex for the whole scan, and on Firestore bills 20k doc reads whether or not the caller needs them all. No way to page or stream.
- **Fix sketch**: Add keyset pagination (`limit` + `after_id` on `list_dataset_items`, ordered by `id`) and pass it through the API and Firestore query. Runners that stream cases can consume pages instead of one giant vec; render truncates to a page.
- **Trade-offs**: Callers that genuinely need all items (a full run) still page through — slightly more request orchestration, but bounded memory and the ability to stream. None material for the common list/preview path.
