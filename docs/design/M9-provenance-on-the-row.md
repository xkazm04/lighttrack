# M9 — Provenance on the row: redaction stamp, revenue FX provenance, typed verdict identity

Size L · gate policy (redaction stamp) / contract (additive columns) · wave B · contexts:
pii-scrubbing, event-ingest, billing-integration, margin-analytics, scoring, rubric-system

Three findings with one shape — a rule acts on every row and records nothing about itself.

## A. Redaction stamp
Problem: `Redactor::redact_event` returns the span count (`crates/api/src/redact.rs` ~171); ingest
logs it at debug and drops it (`crates/api/src/events.rs` ~58-61), so does relay settle
(`relay.rs` ~294-302). The rule list is an unversioned `OnceLock<Vec<Rule>>`
(`crates/anon/src/lib.rs` ~32-77) that has already changed shape once (D14 addendum). A database is
an indistinguishable mix of raw and scrubbed rows; D14 itself says a scrub that rewrote judged
evidence is "the one class of defect this product cannot observe". `dataset_items.anonymization`
already stores `{method, redactions}` (schema ~236) — the shape is accepted; only the fact table
lacks it. `METADATA_PASSTHROUGH` (`redact.rs` ~215) already defines server-owned keys.
Design:
1. `crates/anon`: `pub fn rules_fingerprint() -> &'static str` (sha256 of the ordered pattern
   strings + placeholders, computed once) and `ScrubReport { redactions, by_placeholder }`.
2. `crates/core/src/project.rs`: `RedactionStamp { policy: Redaction, scrub: bool, spans: u32, rules: String }`;
   `LlmEvent::redaction() -> Option<RedactionStamp>` reader beside `customer_id()`.
3. `redact.rs`: after scrubbing, write `metadata.redaction` server-side — strip any client-sent
   value first (exactly like `api_key_id`), add `"redaction"` to `METADATA_PASSTHROUGH`. Relay
   settle path gets the same stamp. `log_posture` prints the fingerprint.
4. Store: `EventFilter += redaction_rules: Option<String>, min_redacted_spans: Option<u32>`
   (SQLite/PG JSON-path filters like the existing ones at `sqlite/events.rs` ~206-220; Firestore
   client-side like `scope_value` or `unsupported_extension()`); `redaction_posture(project, since)
   -> Vec<(Option<RedactionStamp>, u64)>` grouped read on all three backends (Full or `Unsupported`;
   conformance asserts stamp round-trip + grouping). `GET /v1/projects/:id/redaction` (admin/project).
5. Scoring: `ScoreDetail += evidence_redacted_spans: Option<u32>` copied from the event at judge
   time (`runner/score.rs`, `score_traces.rs`); rubric report notes when > 0.
6. `docs/DATA_MODEL.md` server-owned key; `docs/DECISIONS.md` D14 addendum "the scrub records itself".
   Correlation-preserving placeholders are a follow-on, not this item.

## B. Revenue FX provenance
Problem: `FxTable::to_usd` returns `{amount_usd, converted}` (`crates/billing/src/fx.rs` ~118-137)
but both adapters keep only `.amount_usd` (`stripe.rs` ~151/178, `polar.rs` ~172/244); `RevenueEvent`
has no `amount_minor`, `fx_rate`, `fx_book_version`, `converted` (`core/revenue.rs` ~43-74; schema
~242-255). `unconverted_currencies` is computed per request against the **live** FX table
(`api/revenue.rs` ~91-100), so adding a missing rate later hides the caveat while stored 1:1 rows
stay wrong. Upsert overwrites `amount_usd` on redelivery (`sqlite/revenue.rs` ~23-27) — silent
restatement.
Design:
1. `RevenueEvent += amount_minor: Option<i64>, fx_rate: Option<f64>, fx_book_version: Option<String>, converted: Option<bool>`
   (serde defaults; old rows: `converted` inferred at read = currency is base). Additive columns
   on SQLite (`ADDED_COLUMNS`), PG (`ALTER … IF NOT EXISTS`), Firestore fields; codec round-trip
   tests in each backend.
2. `FxTable::version()` (from `_meta.last_verified` or a content hash); `UsdAmount += rate`;
   adapters populate the four fields.
3. `unconverted_currencies` reads `r.converted == Some(false)` — one predicate change.
4. `Store::reprice_revenue(project, currency, rate, version, dry_run) -> RepriceReport { matched, changed }`
   updating only rows with `converted = false`, stamping the new version; SQLite + PG; Firestore
   `Unsupported`. `POST /v1/revenue/reprice?currency=GBP&dry_run=true` (admin; never over MCP).
   CLI `lt usage reprice`.
5. Upsert on redelivery updates `amount_usd` only when `excluded.amount_minor <> amount_minor`.
6. `docs/CURRENCY.md`: replace "re-ingest" with the reprice procedure.

## C. Typed verdict identity
Problem: `Score.rubric` is one free-text column with six encodings and no `rubric_id`
(`core/score.rs` ~164; writers: `runner/judge_spec.rs` ~64-69, `bench.rs` ~333 `bench:{name}`,
`compare.rs` ~533 `{name}:{label}#case{i}`, `pairwise.rs` ~271, `calibrate_watch.rs` ~100
`lt:calibration:`). `score_drop` keys its window on `(project, rubric)` (`api/alerts.rs` ~385) so
compare/pairwise scores each get a unique key and never accumulate; trace idempotency matches on
the rubric name (`score_traces.rs` ~254-256). `Rubric` has no `version`/`supersedes`
(`core/rubric.rs` ~124-137).
Design:
1. `Score += rubric_id: Option<String>, kind: ScoreKind { Freeform, Rubric, BenchCase, CompareCell, PairwiseGame, Calibration, Trace }`
   (serde default `Freeform`); additive columns SQLite/PG, Firestore fields; `list_scores` gains
   `rubric_id`/`kind` filters (SQLite/PG; Firestore client-side or `Unsupported`).
2. `Rubric += version: u32 (default 1), supersedes: Option<String>`; `POST /v1/rubrics/:id/versions`
   (copy-with-changes, new id, linked). Do **not** add the calibration-gated `active` flag here (M11).
3. Every runner producer stamps `kind` + `rubric_id`; the legacy `rubric` string stays verbatim.
4. `alerts.rs::record_score`: key on `rubric_id` when present; roll `BenchCase`/`CompareCell`/
   `PairwiseGame` up under the benchmark id instead of per-case keys.
5. Collective `rubric_fingerprint` derives from `(rubric_id, version)` when present (`api/collective/scorecard.rs`).
6. `GET /v1/scores?rubric_id=&kind=`; MCP `list_scores` gains the two filters; CLI `lt rubrics version`.

## Out of scope
Labels/calibration store (M11). Grouped score summaries (M23).

## Gates
`cargo build/test/clippy` for lighttrack-anon, -core, -billing, -store, -store-pg,
-store-firestore, -api, -runner, -mcp, -cli; SQLite conformance; codec round-trip tests per
backend for the new columns.

## Evaluation
Before: 0 redaction facts on rows; FX `converted` dropped ×4 sites, caveat from live table; 6
rubric encodings, `rubric_id` absent, per-case alert keys. After: `GET /v1/projects/:id/redaction`
reports posture by rules version; `reprice --dry_run` reports N rows; `GET /v1/scores?kind=bench_case`
works and `score_drop` can fire on a benchmark's case stream.
