## What & why

<!-- One or two sentences. Link the issue if there is one. -->

## Crates touched

<!-- e.g. crates/api, crates/store, crates/store-pg -->

## Tests run

<!--
Paste the commands you actually ran, not the ones you meant to. `cargo build -p <crate>` is not a
test. If you ran nothing, say so — that is an honest answer for a docs change and a red flag for a
store change.
-->

```
```

## Backend parity

Backend parity is a correctness property here, not a nicety: a `Store` method that SQLite implements
and another backend quietly defaults is how caps and filters silently become advisory.

- [ ] Not applicable — this PR does not touch the `Store` trait or any backend
- [ ] Implemented in **all three** backends (SQLite, Postgres, Firestore)
- [ ] Implemented in some; the rest return `StoreError::Unsupported` (→ 501), never a quiet default
- [ ] The store-conformance suite was extended to cover the new behavior

## Checklist

- [ ] Every touched file is still under ~300 LOC, or was split by responsibility
- [ ] No business logic added to a binary's `main.rs` (wiring only)
- [ ] `cargo fmt` run on the files I touched; no new clippy warnings
- [ ] No secrets, keys, or `.env` content in the diff
- [ ] Docs in `docs/` updated if this changes behavior an operator can see
