#!/bin/sh
# Every blocking gate, runnable locally, in one place.
#
# Why this exists: until 2026-08-24 there was no local rung at all — no hooks, no script — so every
# check's first run was a CI round trip. That is the expensive way to learn that `cargo fmt` wants a
# blank line: push, wait for a runner to boot, read a red board, fix, push again. The remote run
# should be a CONFIRMATION of something you already know, not the discovery.
#
# THE COMMANDS ARE NOT COPIED HERE. Each line below is the `command` string from the matching
# capability in `.ai/manifest.yaml`, and `crates/core/tests/manifest_guard.rs` fails the build if any
# capability in `controls.ciHardPass` is missing from this file or spelled differently. So the ladder
# has one authority: add a blocking gate to the manifest and this script is red until it runs it too.
# (`.github/workflows/ci.yml` remains the authority on what BLOCKS; the manifest projects it, and
# CONTRIBUTING.md's table is checked against it by the same suite.)
#
# The three outcomes stay distinguishable — passed, failed, and COULD NOT RUN. A gate whose engine is
# missing on this machine announces itself and does not fail the run: a local rung that punishes a
# fresh clone teaches people to bypass it, and the bypass habit outlives the missing tool. CI
# installs every engine itself and blocks unconditionally, which is where the guarantee lives.
#
#   sh scripts/gates.sh            # everything
#   sh scripts/gates.sh --fast     # the cheap, always-available half (fmt, clippy, workspace tests)
#
# Wire it into the local ladder:  git config core.hooksPath .githooks
set -u

FAST=0
[ "${1:-}" = "--fast" ] && FAST=1

failed=""
skipped=""

# Run one gate. $1 = capability name, $2 = the required engine (empty when it is cargo), $3 = command.
gate() {
    name=$1
    engine=$2
    cmd=$3
    if [ -n "$engine" ] && ! command -v "$engine" >/dev/null 2>&1; then
        printf '  SKIP  %-26s (%s is not installed on this machine)\n' "$name" "$engine"
        skipped="$skipped $name"
        return 0
    fi
    printf '  ....  %-26s %s\n' "$name" "$cmd"
    if sh -c "$cmd" >/dev/null 2>&1; then
        printf '\033[1A  PASS  %-26s %s\n' "$name" "$cmd"
    else
        printf '\033[1A  FAIL  %-26s %s\n' "$name" "$cmd"
        # Re-run visibly: the whole point of a local rung is reading the error here rather than in a
        # CI log ten minutes from now.
        sh -c "$cmd"
        failed="$failed $name"
    fi
}

echo "local gates (the same commands CI blocks on):"

gate format-check "" "cargo fmt --all -- --check"
gate lint "" "cargo clippy --workspace --all-targets -- -D warnings"
gate test "" "cargo test --workspace"

if [ "$FAST" -eq 0 ]; then
    gate conformance "" "cargo test -p lighttrack-store --test sqlite_conformance"
    gate chart-policy "" "cargo test -p lighttrack-core --test chart_policy_guard"
    gate judge-eval "" "cargo test -p lighttrack-engine judge::tests::corpus"
    gate audit-policy cargo-deny "cargo deny --locked check bans licenses sources"
    gate audit-secrets gitleaks "gitleaks git . --config .gitleaks.toml --redact --no-banner"
    gate test-client-rust "" "cargo test --manifest-path clients/rust/Cargo.toml"
    gate test-client-python python "python -m pip install ./clients/python && python -m unittest discover -s clients/python/tests"
    gate test-client-typescript npm "cd clients/typescript && npm ci && npm run build && npm test"
fi

echo
[ -n "$skipped" ] && echo "not run (engine missing locally; CI runs these unconditionally):$skipped"
if [ -n "$failed" ]; then
    echo "FAILED:$failed"
    exit 1
fi
echo "all local gates passed."
