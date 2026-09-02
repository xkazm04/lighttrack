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

## Follow-on, not scheduled: `lt-gateway`

**Recorded by M5 (2026-09-02), which shipped parts A and B of its brief and deliberately did not
ship part C.** M5's SDK-side admission closes the pre-spend gap for traffic that goes through a
LightTrack SDK: the client caches what the server last said about the project's caps and refuses a
call locally before it is made. What it cannot cover is everything that does not import an SDK — a
`curl` in a cron job, a third-party agent framework, a service written in a fourth language, a
provider call made by a vendor's own SaaS on the customer's key. The proposed `lt-gateway` is an
inline reverse proxy in front of the provider endpoint that admits *every* call regardless of who
made it, and records the usage it already has to parse. That is a different product surface with
different failure modes from anything here — it sits on the critical path of the customer's LLM
traffic, so its availability becomes theirs, and `docs/ARCHITECTURE.md` §4 deferred it on exactly
that ground ("adds latency and a critical-path dependency"). Shipping it needs its own DECISIONS
entry answering the questions M5 got to dodge: what happens when the gateway is down (fail open and
lose the cap, or fail closed and take the app down with it), whether it terminates TLS to the
provider or streams opaquely, how it is authenticated separately from ingest, and whether an
operator can be prevented from pointing it at a provider it will then hold credentials for. Out of
scope for this wave, and out of scope for M5 in particular — recorded here so it is a deferred
decision rather than a forgotten one.
