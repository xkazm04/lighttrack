# Design docs — moonshot build waves (2026-09-02)

Source: the moonshot-architect deck (vault `ScanSweep/moonshot-2026-09-01.md`), 24 accepted items.
Each `M<n>-*.md` here is the build brief one Opus subagent implements in its own git worktree; the
coordinator merges wave by wave and re-runs the gates on the merged tree. Waves are composed so the
five write sets inside a wave are disjoint at the file level (the `Store` trait in
`crates/store/src/lib.rs` is the one shared file — each item appends its own block and never
reformats the file).

| Wave | Items | Why together |
|---|---|---|
| A | M1 parity manifest · M8 canonical identity · M6 one Claude seam · M16 tenancy lifecycle · M30 SDK contract fixtures | foundations; no item depends on another accepted item |
| B | M2 rollup primitive · M7 typed job queue + fenced relay lease · M9 provenance on the row · M20 collective hub parity · M4 measure→act margin policy | trait-heavy items, each in its own domain block; M1's manifest is merged so each declares its surface |
| C | M26 unpriced ledger (after M2) · M27 forecast honesty (after M2, M4) · M10 resolvable eval targets · M3 alert ledger · M18 device fleet (after M7) | consumers of wave B's primitives |
| D | M5 pre-spend admission (after M30) · M11 label ledger + judge trust (after M10) · M19 relay judgeable (after M18) · M22 contribution ledger (after M20) · M23 prompt canary (after M10) | leaf features |
| E | M17 scope on every Store read · M24 eval corpus lineage | M17 rewrites read signatures across handlers, so it runs after the handler-adding waves |
| F | M14 schema-as-data · M15 typed API contract registry | both generate from a single source and must see every table and route the earlier waves added |

Rules every design inherits (from CLAUDE.md): ≤ ~300 LOC per file; `main.rs` wiring only; a
`Store` method one backend implements and another silently defaults is a bug — implement or
return `StoreError::Unsupported`; fixed-width RFC3339 timestamps; judge unbudgeted; MCP writes
gated. Gates per crate touched: `cargo build -p`, `cargo test -p`, `cargo clippy -p --all-targets
-- -D warnings`, `rustfmt`; `cargo test -p lighttrack-store --test sqlite_conformance` for store
changes; `cargo test -p lighttrack-core --tests` when `.ai/` or `context-map.json` change.
