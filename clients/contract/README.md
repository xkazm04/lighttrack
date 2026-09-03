# The SDK contract

One machine-readable description of the behaviour every LightTrack client SDK must share, plus the
capability manifests that say where they legitimately differ.

## Why

The Python, TypeScript and Rust clients were three hand-synchronised implementations of one
contract. The provider extractors were triplicated. The PII table for `guard({ noPII })` was
triplicated byte-for-byte — and then the server's table moved (D14 replaced a phone regex that
matched every ISO date) and the three copies did not, so a client-side guard and the ingest scrubber
could disagree about what counts as PII with nothing anywhere saying so. CI ran the three suites as
unrelated jobs that could not see drift between them.

Shared vectors turn "we believe these agree" into a test.

## Layout

| File | What it fixes |
|---|---|
| `schema.json` | The shape of every fixture file, and the vocabulary (check keys, PII kinds). |
| `fixtures/extractors.json` | Real captured provider responses -> `{model, input, output, cached}`. |
| `fixtures/guard.json` | Outputs + rules -> `{ok, violations}` as sorted **check keys**. |
| `fixtures/journal.json` | A journal file body -> its unsettled records. |
| `fixtures/limits.json` | Ingest responses / 429s -> the parsed limit view. |
| `fixtures/diagnostics.json` | HTTP status -> rate-limiting bucket + the load-bearing hint text. |
| `fixtures/pii.json` | **Generated** from `crates/anon` by `cargo test -p lighttrack-anon`. |

Runners: `clients/rust/tests/contract.rs`, `clients/typescript/src/contract.test.ts`,
`clients/python/tests/test_contract.py`. Each loads every file and asserts. CI runs all three plus
the anon export check and the matrix diff in one blocking `sdk-contract` job.

## Rules

- **A behaviour not in a fixture is not part of the contract**; a behaviour that is may not differ
  between languages. Add a case before you add a behaviour.
- **Messages are prose, keys are contract.** Violation *messages* may read naturally per language;
  the check keys (`max_words`, `pii:phone`, `not_match:<pattern>`) may not.
- **Regex stays in the RE2 / JS / Python / Rust common subset** — no lookaround, no backreferences.
  The anon export test enforces it, because RE2 and Rust's `regex` reject what JS and Python accept.
- **`pii.json` is generated, never hand-edited.** Change `crates/anon/src/lib.rs`, then
  `LIGHTTRACK_UPDATE_FIXTURES=1 cargo test -p lighttrack-anon`, then re-run the three SDK suites.
- **A capability an SDK does not have is declared, not silently unasserted.** Each
  `clients/<lang>/lighttrack.manifest.json` marks it `not_supported` with a note; the runner skips
  that fixture and `scripts/gen-sdk-matrix.mjs` renders the gap into `clients/README.md`.

## Regenerating

```bash
LIGHTTRACK_UPDATE_FIXTURES=1 cargo test -p lighttrack-anon            # fixtures/pii.json
LIGHTTRACK_UPDATE_FIXTURES=1 npm test --prefix clients/typescript     # clients/typescript/src/pii.ts
LIGHTTRACK_UPDATE_FIXTURES=1 python -m pytest clients/python/tests    # clients/python/lighttrack/pii.py
node scripts/gen-sdk-matrix.mjs                                       # clients/README.md matrix
```
