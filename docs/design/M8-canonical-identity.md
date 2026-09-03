# M8 — Canonical provider and model identity

Size XL · gate contract · wave A · contexts: event-ingest, cost-pricing, limit-enforcement,
judge-engine, model-leaderboard (+ store row mappers)

## Problem
`Provider` is a closed enum `{OpenAi, Anthropic, Google, #[serde(other)] Unknown}`
(`crates/core/src/event.rs` ~8-33). Every unmodeled provider (mistral, bedrock, groq, deepseek,
ollama, cohere…) is stored as the literal `'unknown'` (`crates/store/src/sqlite/events.rs` ~60) and
the raw name is lost. `PriceBook::key(provider, model)` formats `provider.as_str()` → `unknown/<m>`
while `from_rows` keys DB rows by the raw column → `mistral/<m>` (`crates/core/src/pricing.rs`
~94 vs ~104), so `PUT /v1/prices/mistral/x` succeeds and is unreachable forever — while the 429
text tells the operator to do exactly that (`crates/api/src/events_admission.rs` ~412-416). Limit
scopes collapse all unmodeled providers into one bucket. OTLP infers provider by substring
(`crates/api/src/otlp/semconv.rs` ~255-264). Separately, model identity is canonicalized four ways:
`Provider::from_wire`, `trim_date_suffix` (pricing.rs ~238), the `config/model_aliases.json` table
(`crates/core/src/collective/aliases.rs`), and `judge_provider_of`/`canon_judge`
(`crates/api/src/collective/scorecard.rs` ~67-96, `sanitize.rs` ~108-117). 7 of the 8 alias
targets are models absent from `config/pricing.json`; 0 of 15 priced models have an alias.

## Design
1. `crates/core/src/provider.rs` (new): `ProviderId(String)` newtype — serde transparent,
   canonicalize on deserialize (lowercase, trim, allowlist `[a-z0-9._-]`, empty → `"unknown"`);
   `ProviderFamily { OpenAi, Anthropic, Google, Mistral, Meta, Other }` + `pub fn family_of(&str)`
   (prefix/synonym table: `claude|anthropic`→Anthropic, `openai|azure-openai|azure`→OpenAi,
   `google|gemini|vertex|google-vertex`→Google, …). Tests: `mistral` round-trips as `mistral`;
   `az.ai.openai` → id `az.ai.openai`, family OpenAi.
2. `crates/core/src/model_id.rs` (new): `ModelId { provider: ProviderId, family: String, variant: Option<String>, lane: Option<String> }`
   and `canonicalize(provider: &str, model: &str) -> ModelId` — one algorithm: provider synonyms,
   date-suffix trim (absorb `trim_date_suffix`), `@batch/@flex/@in>N` lane split, then the alias
   table. `judge_family(spec: &str) -> ProviderFamily` absorbs `judge_provider_of` + `canon_judge`.
3. `LlmEvent.provider: ProviderId`. Remove the `Provider` enum; keep a `#[deprecated] pub type Provider = ProviderId`
   shim only if `clients/rust` needs it for one release (check `clients/rust/src/lib.rs` — it
   reuses `lighttrack_core::LlmEvent`). `scope_dims().provider` reads the raw id.
4. Pricing: `PriceBook::key(&str, &str)`; `lookup`/`resolve*` take `&str` and go through
   `canonicalize` first; add the declared-alias step to resolution order (exact → lane → tier →
   date-trim → alias table). Merge `config/model_aliases.json` into the price seed: each model in
   `config/pricing.json` gains `"aliases": [...]`; `ModelAliases::from_json_str` becomes a shim over
   the new shape. **Test**: every alias target is a priced family. Seed rows for at least
   `mistral/mistral-large`, `deepseek/deepseek-chat`, `groq/llama-3.3-70b` to prove the open key path.
5. Store: row mappers stop `parse_enum::<Provider>` and read the string (`sqlite/events.rs` ~876,
   `store-pg/src/events/cols.rs`, `store-firestore/src/events.rs`). Historical `'unknown'` rows stay
   `unknown` — document as the backfill sentinel in `docs/DATA_MODEL.md`. Conformance case: event
   with provider `mistral` reads back as `mistral` and prices from a `mistral/*` row.
6. OTLP `provider_of` returns the canonical raw id; substring family matching moves to `family_of`.
7. Engine: `model_family`/`same_family` consume `ProviderFamily`; judge spec `[provider/]model`
   accepts any id and routes to an adapter by family (unknown family → clear "no generation
   adapter" error).
8. Collective: digest/sanitize produce `provider` from `ModelId`; keep the conservative alias
   allowlist keyed on raw id — never merge on family; `judge_provider = judge_family(..).as_str()`.
   Delete `scorecard::judge_provider_of`, `sanitize::canon_judge`, `aliases.rs` provider map;
   retarget their tests. `LimitScope::Model` compares on `ModelId` (scoped cap on `gpt-4o` also
   catches `gpt-4o-2024-08-06`).

## Out of scope
Rollup/aggregate query changes (M2). Any schema change (column is already TEXT).

## Gates
`cargo build/test/clippy -p lighttrack-core -p lighttrack-store -p lighttrack-store-pg
-p lighttrack-store-firestore -p lighttrack-api -p lighttrack-engine -p lighttrack-runner
-p lighttrack-mcp`; `cargo test -p lighttrack-store --test sqlite_conformance`;
`cargo build -p lighttrack` (the Rust client SDK in `clients/rust`) if it compiles against core.
Dashboards keyed on the three literals: note in `docs/DATA_MODEL.md`.

## Evaluation
Before: 4 canonicalizers; `PUT /v1/prices/mistral/x` never matches a lookup; 7/8 alias targets
unpriced. After: 1 canonicalizer; the same PUT prices the next `mistral` event (`cost_source=book`);
alias-target ∈ price-book test = 100%.
