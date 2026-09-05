# AGENTS.md — pointer, not a second answer

**The canonical agent guidance for this repository is [`CLAUDE.md`](CLAUDE.md)**, declared as
`guidance.canonical` in [`.ai/manifest.yaml`](.ai/manifest.yaml). Read that file — it is
tool-neutral working agreement (code structure, Rust idioms, build/test operating advice, secrets,
parallel-session rules, the invariants not to regress), not Claude-specific ceremony.

**The commands** — build, test, lint, format-check, conformance, the audits — are written down
exactly once, in `capabilities:` in [`.ai/manifest.yaml`](.ai/manifest.yaml).
`crates/core/tests/manifest_guard.rs` holds `scripts/gates.sh` to those exact strings, and
`crates/core/tests/blocking_gate_guard.rs` holds `.github/workflows/ci.yml` to the manifest's claim
about which of them *block*. A command copied into prose is a command that goes stale silently —
that is exactly how this repository ended up with a second guidance file prescribing an npm build
for a Rust workspace that has none.

This file exists because a second tool ecosystem should reach the same canonical document rather
than the first file it happens to open. It restates nothing on purpose, and
`crates/core/tests/guidance_guard.rs` fails `cargo test --workspace` if it ever grows a build
command or stops pointing at the canonical file.
