//! The trust-base census line for environment writes: the members a census by `unsafe` cannot see.
//!
//! This workspace is on edition 2021, where `std::env::set_var` and `remove_var` are safe calls.
//! Edition 2024 re-marked both `unsafe` because their precondition (no other thread reads or writes
//! the environment, by any route, while the call runs) is one no checker can verify. In 2021 code
//! the precondition still holds and only the marker is missing, so on 2026-10-14 a census by marker
//! reported this tree's trust base as empty (0 sites) beside 49 unmarked writes in 15 test files
//! (registry technique `trust-base/census-counts-unmarked-members`).
//!
//! So this guard reports two counts, each with its predicate, and holds the second to one place:
//!
//! * **A, marker sites:** non-comment lines of `.rs` files under `crates/` containing `unsafe`
//!   followed by `{`, `fn`, `impl`, `extern` or `trait`.
//! * **B, unmarked environment writes:** non-comment lines of `.rs` files under `crates/`
//!   containing `set_var(` or `remove_var(`. Every one must sit in `crates/core/src/test_env.rs`,
//!   the body of the one lock every environment-writing test takes; a write anywhere else is red.
//!
//! B is the text stand-in for the compiler's own count, the migration lint
//! (`cargo clippy --workspace --all-targets -- -W deprecated_safe_2024`), which re-checks every
//! member and is too slow for this slot. On 2026-10-14 the two named the same 49 lines. The
//! stand-in holds only while the workspace is on 2021: after a migration every write sits inside an
//! `unsafe` block, and the census reads A instead.
//!
//! `cargo test -p lighttrack-core --test env_write_census -- --nocapture` prints the census line.

use std::path::{Path, PathBuf};

const WRITES: &[&str] = &["set_var(", "remove_var("];
const MARKERS: &[&str] = &[
    "unsafe {",
    "unsafe fn",
    "unsafe impl",
    "unsafe extern",
    "unsafe trait",
];
/// The only file allowed to write: the shared lock's own body.
const SHARED_LOCK: &str = "crates/core/src/test_env.rs";
/// The census never counts its own needles.
const THIS_FILE: &str = "crates/core/tests/env_write_census.rs";

fn repo_root() -> PathBuf {
    // crates/core -> repo root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the repo root resolves")
}

/// `path:line` (repo-relative, forward slashes) of every hit under `crates/`, sorted.
fn census(root: &Path, needles: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    walk(&root.join("crates"), root, needles, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, root: &Path, needles: &[&str], out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != "target") {
                walk(&path, root, needles, out);
            }
            continue;
        }
        if path.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel == THIS_FILE {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        out.extend(
            hits(&text, needles)
                .into_iter()
                .map(|n| format!("{rel}:{n}")),
        );
    }
}

/// 1-based numbers of the lines that are code, not a comment, and contain a needle.
fn hits(text: &str, needles: &[&str]) -> Vec<usize> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim_start().starts_with("//"))
        .filter(|(_, l)| needles.iter().any(|n| l.contains(n)))
        .map(|(i, _)| i + 1)
        .collect()
}

#[test]
fn every_environment_write_takes_the_shared_lock() {
    let root = repo_root();
    let markers = census(&root, MARKERS);
    let writes = census(&root, WRITES);
    let outside: Vec<&String> = writes
        .iter()
        .filter(|w| !w.starts_with(&format!("{SHARED_LOCK}:")))
        .collect();
    println!(
        "trust-base census, crates/ on edition 2021: A marker sites (`unsafe` block/fn/impl/extern/\
         trait) = {}; B unmarked environment writes (`set_var(`/`remove_var(` calls) = {}, outside \
         the shared lock = {}",
        markers.len(),
        writes.len(),
        outside.len()
    );
    assert!(
        outside.is_empty(),
        "the environment is written outside the shared lock: {outside:?}\n\
         In a test, take `lighttrack_core::test_env::EnvGuard` (the `test-env` feature, from \
         [dev-dependencies]) and write through it, so the test serializes against every other test \
         that writes the environment. In shipped code the write is a trust-base member: move it \
         before the first thread starts, or pass the value explicitly instead."
    );
}

/// The census must be able to find something, or a walker that lost the tree reads as a clean one.
#[test]
fn the_census_can_go_red() {
    let writes = census(&repo_root(), WRITES);
    assert!(
        writes.iter().any(|w| w.starts_with(SHARED_LOCK)),
        "the walker did not find the shared lock's own writes, so it is not reading the tree: \
         {writes:?}"
    );
    let sample = "fn f() {\n    // std::env::set_var(\"K\", \"v\") is the call\n    \
                  std::env::set_var(\"K\", \"v\");\n    std::env::remove_var(\"K\");\n}\n";
    assert_eq!(
        hits(sample, WRITES),
        vec![3, 4],
        "a call counts, a comment does not"
    );
    let markers = "let unsafe_advice = 1;\nunsafe { f() }\nunsafe impl Send for T {}\n";
    assert_eq!(hits(markers, MARKERS), vec![2, 3]);
}
