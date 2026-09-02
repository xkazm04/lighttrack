# M30 — One cross-language SDK contract: shared fixtures, one conformance suite, generated matrix

Size L · gate none · wave A · contexts: client-sdks, pii-scrubbing (export only), CI

## Problem
The Python, TypeScript and Rust SDKs are three hand-synchronised implementations of one contract.
Provider extractors are triplicated (`clients/typescript/src/index.ts` ~91-112,
`clients/python/lighttrack/client.py` ~60-81, `clients/rust/src/lib.rs` ~164-191); the guard PII
regex table is triplicated byte-for-byte (`index.ts` ~140-145, `client.py` ~86-91, `lib.rs`
~406-411) and its `phone` rule `(?:\+?\d[\s().-]?){10,}` is the pre-D14 shape that matches ISO
dates — the server fixed it (`docs/DECISIONS.md` D14 addendum), the SDKs did not; the journal
record format is duplicated (`journal.ts`, `journal.py`) and the Rust SDK has no journal, no
`instrument`, no relay client (`lib.rs` ~20) while `clients/typescript/README.md` ~49-67 promises
crash-surviving breadcrumbs generically. CI runs the three SDK suites as unrelated jobs
(`.github/workflows/ci.yml` ~210-290) that cannot see drift between them.

## Design
1. `clients/contract/` (new): `schema.json` + `fixtures/{extractors,guard,journal,limits,diagnostics,pii}.json`.
   - `extractors.json`: real captured provider responses (OpenAI chat + responses + embeddings,
     Anthropic messages incl. `cache_read_input_tokens`, Gemini camelCase and snake_case duals) →
     expected `{model, input_tokens, output_tokens, cached_input_tokens}`.
   - `guard.json`: inputs → expected `{ok, violations:[kind]}` including ISO dates and semver
     strings that must **not** flag as phone.
   - `journal.json`: a journal file body → expected unsettled records.
   - `limits.json`: ingest responses / 429s (`usage_ratio`, `shed_fraction`, `Retry-After`) →
     expected parsed view (M5 will consume; here only parse expectations).
   - `diagnostics.json`: HTTP status → expected diagnostic kind string.
   - `pii.json`: **exported from the server**: add a `#[test]` in `crates/anon` that renders the
     rule set (`kind`, pattern, placeholder, ordered) to `clients/contract/fixtures/pii.json` and
     fails when the checked-in file differs (stale-check pattern). The SDK guards load their
     patterns from this file at build/test time (TS: import JSON; Py: `json.load`; Rust:
     `include_str!`) so the client `noPII` guard can no longer contradict the ingest scrubber.
     Regex dialect: keep patterns to the RE2/JS/Python/Rust common subset; the anon test asserts
     each pattern compiles in Rust `regex` and contains no lookaround/backrefs.
2. Fixture runners: `clients/rust/tests/contract.rs`, `clients/typescript/src/contract.test.ts`,
   `clients/python/tests/test_contract.py` — each loads every fixture file and asserts. Fix the
   divergences the fixtures expose (at minimum the phone regex in all three; Gemini
   `modelVersion`/`model_version` handling).
3. Capability manifest per SDK: `clients/{typescript,python,rust}/lighttrack.manifest.json`
   declaring `{track, span, journal, instrument, guard, relay, admit, batch}` → `supported |
   not_supported | planned`. `scripts/gen-sdk-matrix.mjs` (or `.py`) renders `clients/README.md`'s
   capability table from the three manifests; CI diff-checks it. Rust gaps (journal, relay,
   instrument) are marked honestly `not_supported` — do not implement them in this item.
4. CI: one `sdk-contract` job in `.github/workflows/ci.yml` running the three fixture suites plus
   the anon export check and the matrix diff; keep the existing per-language jobs. Advisory or
   blocking: match the repo's existing philosophy (blocking for the Rust suite, advisory for lint —
   make the contract job **blocking**, it is a test).
5. Update `clients/typescript/README.md` (and Python/Rust READMEs) to point at the generated
   matrix instead of prose claims.

## Out of scope
SDK-side admission (`admit()`, M5). New SDK features in Rust.

## Gates
`cargo test -p lighttrack` (Rust SDK crate name — check `clients/rust/Cargo.toml`), `cargo test -p
lighttrack-anon`, `npm test` in `clients/typescript`, `python -m pytest clients/python/tests`; the
matrix diff script exits 0.

## Evaluation
Before: 3 copies of extractors + PII table; pre-D14 phone regex live ×3; 0 shared vectors; Rust
gaps undocumented. After: fixture suite green in three languages; PII patterns single-sourced from
`crates/anon`; generated matrix; date/semver guard cases pass.
