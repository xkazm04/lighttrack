# .claude/CLAUDE.md — pointer, not a second answer

**The canonical agent guidance for this repository is [`CLAUDE.md`](../CLAUDE.md)**, declared as
`guidance.canonical` in [`.ai/manifest.yaml`](../.ai/manifest.yaml). Read that file: it is the
working agreement — code structure, Rust idioms, build/test operating advice, secrets, the
parallel-session rules, and the invariants not to regress.

**The commands** — build, test, lint, format-check, conformance, the audits — are written down
exactly once, in `capabilities:` in [`.ai/manifest.yaml`](../.ai/manifest.yaml).
`crates/core/tests/manifest_guard.rs` holds `scripts/gates.sh` to those exact strings, and
`crates/core/tests/blocking_gate_guard.rs` holds `.github/workflows/ci.yml` to the manifest's claim
about which of them *block*. A command copied into prose is a command that goes stale silently.

## What used to be here, and why this file is a pointer now

The project generator wrote its own scaffold into this file: a JavaScript build instruction for a
workspace that has no JavaScript build, next to "Add your project description here". This repository
then had two files an agent might open first, and "how do I build this" was answered by whichever one
it opened — the answer that loses being the one nobody knows they lost.

The mitigation was quarantine: the manifest listed this file under `guidance.staleProjections`, and
the canonical document carried a warning naming it, so a reader who opened the right file first was
told which one to disbelieve. The real fix was always to replace the body rather than annotate it.

Replaced with this pointer on 2026-09-14 and dropped from `staleProjections` in the same commit —
which is the point: `crates/core/tests/guidance_guard.rs` only applies the pointer rules to a
projection that is *not* declared stale, so this file must from now on name the canonical document
and carry no command line, or the build goes red.

One correction worth recording, since the old note in the manifest asserted the opposite and cost two
sessions their attempt: writing this path is not blocked. The claim that the agent harness refuses
edits under `.claude/` was wrong, and it had hardened into a documented reason not to try.
