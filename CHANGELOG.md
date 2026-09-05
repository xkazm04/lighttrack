# Changelog

What changed, release to release, for the people who have to keep something in sync with it.

That last clause is why this file exists rather than "because projects have changelogs". One HTTP API
here is consumed by **three** client SDKs (`clients/python`, `clients/typescript`, `clients/rust`) and
by self-hosted deployments nobody in this repository can see. `clients/contract/openapi.baseline.json`
already records the published *shape* mechanically, and `crates/api/src/openapi.rs` fails the build
if a name leaves that shape without first being marked deprecated — but a machine-readable diff of
400 kB of JSON answers "what moved", never "should I care". This file is the second half: the
human-readable reason, in release order.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions are the `v*` tags that
[`release.yml`](.github/workflows/release.yml) and [`docker.yml`](.github/workflows/docker.yml) build
from. Grouped under `Added` / `Changed` / `Deprecated` / `Removed` / `Fixed` / `Security`.

**Where the entries before this file are.** This changelog starts here rather than being
back-filled: reconstructing per-release notes after the fact from a commit log produces something
that looks authoritative and is not, which is worse than an honest gap. For anything earlier, the
sources that were actually written at the time are `git log`, the dated entries in
[`docs/DECISIONS.md`](docs/DECISIONS.md), and the design notes in [`docs/design/`](docs/design/).

**Writing an entry.** One line per change a *user of the API, an SDK, or a deployment* would notice —
not one line per commit. An internal refactor with no observable effect belongs in the commit
message, not here. If a change touches the HTTP surface, say which route and which field, because
that is the line an SDK maintainer will grep for. The pull-request template asks about this so the
entry is written while the change is still fresh, not reconstructed at tag time.

## [Unreleased]

### Added

- **Judge eval corpus.** `crates/engine/evals/judge/corpus.json` records whole verdicts — overall,
  pass, per-dimension scores and floor hits, agreement, parse accounting — for known cases, replayed
  through the real prompt/scorer/aggregate path with the provider call canned. It runs inside
  `cargo test --workspace`, so a reworded judge prompt or a changed aggregation can no longer move
  what a score *means* with the suite green. Declared as the `judge-eval` capability in
  `.ai/manifest.yaml` and run by `scripts/gates.sh`.
- **`CODEOWNERS`** (`.github/CODEOWNERS`), so GitHub requests a review by itself instead of relying
  on someone — or some agent — remembering to.
- **This file.**
- **`crates/core/tests/blocking_gate_guard.rs`** — `.ai/manifest.yaml`'s `controls.ciHardPass` /
  `ciAdvisory` grades are now checked against `.github/workflows/ci.yml`'s `continue-on-error:`, so a
  gate the manifest calls blocking cannot quietly become advisory (and the reverse). Runs inside
  `cargo test --workspace`.
- **`AGENTS.md`** — a pointer to the canonical `CLAUDE.md` for the tool ecosystems that read that
  filename, plus `crates/core/tests/guidance_guard.rs`, which fails the build if a declared guidance
  projection carries a command or stops naming the canonical file.

### Security

- **`release.yml` and `docker.yml` no longer grant write workflow-wide.** Both are `contents: read`
  at the top now, with `contents: write` (attaching release assets, uploading signature bundles) and
  `packages: write` (pushing to GHCR) granted only to the jobs that use them — so the token sitting
  beside `cargo build`'s build scripts and proc macros can no longer publish or rewrite anything.

### Changed

- **`.githooks/pre-commit` now also checks formatting**, on the staged `.rs` files only, so a
  `cargo fmt --check` violation is caught at commit rather than after a push and a runner boot.
  `LIGHTTRACK_SKIP_FMT=1` bypasses it; a machine without `cargo` gets an announced skip. The compile
  and test gates stay at `pre-push`, deliberately — see `CONTRIBUTING.md`.
- **`.ai/manifest.yaml` declares `guidance.canonical`.** The root `CLAUDE.md` is the authoritative
  agent guidance; `.claude/CLAUDE.md` is a projection of it and is currently listed under
  `guidance.staleProjections` because it still carries generator scaffold that contradicts it.

<!--
Template for the next release — copy, do not delete:

## [x.y.z] — YYYY-MM-DD

### Added
### Changed
### Deprecated
### Removed
### Fixed
### Security
-->
