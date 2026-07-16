# Feature Scout — Evaluation Datasets

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. Captured-trace datasets dead-end: no way to add ground-truth (`expected`), and event builds auto-freeze
- **Severity**: High
- **Category**: half-implemented
- **File**: `crates/runner/src/dataset.rs:79-91`, `crates/api/src/datasets.rs:86-130`, `crates/store/src/sqlite/datasets.rs:49-75`
- **Scenario**: An operator runs `dataset build` (or the `schedule` loop) to curate real production failures into an eval set — exactly the flagship "build a golden set from what actually happened" workflow the product is positioned around. They then want to fill in the *correct* answer for each sampled case so the LLM-judge can grade candidates against a reference.
- **Root cause**: `build_from_events` populates each item's `input` and captured `output`, but never `expected` (dataset.rs:79-85) — production traces carry no golden reference, so `expected` is always `None`. Worse, the builder calls `/freeze` at the very end (dataset.rs:91), so the dataset is immutable the instant it exists. And there is **no item-update or item-delete path anywhere**: the API exposes only `create/list/get/add_item/list_items/freeze` (datasets.rs), and every store (`sqlite/datasets.rs`, `store-pg`, `store-firestore`) implements only `create_item`/`list_items` — no `update_item`. `add_dataset_item` is also blocked once frozen (datasets.rs:95). Net: an event-sampled dataset can never have its ground truth annotated. `bench.rs:58-66` reads `it.expected` when loading a `dataset_ref`, so those cases silently judge with no reference.
- **Impact**: The single highest-value dataset capability in this product class — turn real captured failures into a *golden* benchmark — is structurally impossible. The events→datasets bridge exists but produces reference-less case sets only usable for freeform "simple mode" judging, not regression-grade golden evaluation.
- **Fix sketch**: (1) Add `update_dataset_item` to the store trait + all three backends (UPDATE by id). (2) Add `PATCH /v1/datasets/{id}/items/{item_id}` in api/datasets.rs to edit `expected`/`context`/`tags`, guarded by `ensure_can_admin` and rejected when frozen. (3) Stop auto-freezing in `build_from_events` (leave the freeze to an explicit human step after annotation), or add an `unfreeze`/"draft" state so sampled sets can be curated before locking.
- **Trade-offs**: Editing pre-freeze is safe (frozen sets stay immutable for reproducibility). Not auto-freezing changes current CLI behavior — gate it behind a `--freeze` flag to preserve the one-shot path.

## 2. No dataset export / import round-trip (JSONL/CSV)
- **Severity**: High
- **Category**: capability-gap
- **File**: `crates/api/src/datasets.rs:105-115`, `crates/render/src/datasets.rs:54-86`, `crates/runner/src/dataset.rs:16-30`
- **Scenario**: A team wants to check a curated golden set into git, share it with another team, seed it into CI, or move it from a local sqlite dev store to the Postgres/Firestore prod store. Competing eval products (Braintrust, Langfuse, LangSmith) all treat dataset import/export from JSONL/CSV as table stakes.
- **Root cause**: Ingest only flows one direction and only from live events (`build_dataset`/`build_from_events`, dataset.rs). There is **no export**: `list_dataset_items` returns JSON over HTTP (datasets.rs:105) but there's no runner subcommand to dump a dataset to a file, and `render/datasets.rs::items` only emits a markdown table that **truncates input to 40 and expected to 32 chars** (datasets.rs:80,67) — lossy, not a data export. There is also no file-based import (only event-sampled creation), so a hand-authored or externally-sourced golden set can't be loaded, and datasets can't be migrated across store backends.
- **Impact**: Golden sets are trapped in whichever backend created them — not shareable, not version-controllable, not portable dev→prod. Blocks the standard eval workflow of maintaining datasets as reviewed artifacts alongside code.
- **Fix sketch**: Add `lt-runner dataset export <id> [--format jsonl|csv]` that pulls `/v1/datasets/{id}/items` and writes one `DatasetItem` per line (already `Serialize`, core/dataset.rs). Add `lt-runner dataset import <file> --project <p> --name <n>` that creates a dataset and POSTs each parsed line to `/v1/datasets/{id}/items`, reusing the existing add-item endpoint.
- **Trade-offs**: None material — reuses existing serialization and endpoints; export is read-only.

## 3. Versioning is declared but inert — no version bump or lineage
- **Severity**: Medium
- **Category**: versioning
- **File**: `crates/core/src/dataset.rs:5-26`, `crates/api/src/datasets.rs:32-40`, `crates/store/src/sqlite/datasets.rs:41-47`
- **Scenario**: A frozen golden set `v1` has shipped as a regression baseline. Later the team curates a few more edge cases (or corrects an `expected`). They want `v2` that supersedes `v1` while keeping `v1` for historical run comparability.
- **Root cause**: The `Dataset` doc comment promises "A versioned, reusable evaluation dataset" and carries a `version: u32` field persisted in all three backends, but it is **write-once = 1**: `create_dataset` hardcodes `version: 1` (datasets.rs:36), and the only mutation op is `set_frozen` — no `bump_version`/`new_version_of` in any store. There is also no lineage field (`parent_id`/`derived_from`) on `Dataset`, so a successor set has no link to its predecessor. Freeze is the sole immutability lever; the only way to "revise" a frozen set today is to create an unrelated dataset with a fresh name (which is what `schedule` does via watermark names).
- **Impact**: The advertised versioning is a dead capability. Teams can't evolve a golden set over time as a tracked lineage — they accumulate disconnected datasets, losing the "which set did run X use, and what changed since" story that makes versioned datasets valuable.
- **Fix sketch**: Add `POST /v1/datasets/{id}/versions` that deep-copies a frozen dataset's items into a new editable dataset with `version = parent.version + 1` and a new `parent_id` field (add to core `Dataset` + all backends/migrations). Surface `version` lineage in `render/datasets.rs::detail`.
- **Trade-offs**: Adds a schema column (nullable `parent_id`) across three stores; low risk since existing rows default to no parent.
