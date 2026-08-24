//! Hold the catch-up markers to their contract, so a recovery lane has something to resume from.
//!
//! Every per-change enforcement system needs a recovery lane. Dismissals accumulate, hooks die,
//! whole surfaces get imported from before the discipline existed — and the repair for accumulated
//! drift is a batch pass: read the current source truth, rewrite the affected documentation
//! wholesale. The difference between a bounded repair and an open-ended rewrite-everything campaign
//! is a small recorded artifact: the **marker** the last full pass left behind, saying exactly what
//! the next one owes.
//!
//! Three markers, one per surface family — `docs/`, the agent contract, the shipped SDK READMEs.
//! Not one global marker: those three drift at different rates and are repaired by different passes,
//! and a shared marker forces the narrowest pass to lie about the widest surface.
//!
//! What this guard enforces:
//!
//! 1. **Every marker parses**, and carries an anchor commit + date. A pass that cannot determine its
//!    range must fail loudly rather than silently defaulting to either extreme — a full rewrite
//!    (expensive surprise) or an empty range (silent no-op).
//! 2. **The denominator holds.** Every file under a family's declared surfaces appears in exactly one
//!    of `covered` / `skipped`. This is what makes "full pass" a predicate rather than a claim: a doc
//!    added tomorrow joins no list, and the build says so in the same change that added it.
//! 3. **Skips are first class.** Each carries a `kind` and a `reason`. A marker that records only
//!    successes reports its own blind spots in the voice of completeness.
//! 4. **The marker records what was done, never what is hoped.** The exemplar failure this technique
//!    is built on is a marker that ended "the per-session hook now prevents this kind of drift; bulk
//!    rewrites should not be needed again" — written on the day the never-fired hook landed, a hope
//!    recorded as a fact in exactly the artifact the next repair pass reads to decide how suspicious
//!    to be. So predictions are rejected mechanically here.
//!
//! `include_str!` rather than a runtime read for the markers themselves: a marker that has been moved
//! or deleted is a BUILD error, not a skipped check.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

const DOCS: &str = include_str!("../../../docs/catchup-marker.json");
const AGENT: &str = include_str!("../../../.ai/catchup-marker.json");
const CLIENTS: &str = include_str!("../../../clients/catchup-marker.json");

/// Phrases that turn a record into a prediction. The marker's job is "what this pass did and against
/// what"; claims about the future belong to the enforcement's own liveness evidence, which is
/// measured rather than promised.
const PREDICTIONS: &[&str] = &[
    "should not be needed again",
    "will not happen again",
    "cannot happen again",
    "will prevent",
    "prevents this kind of drift",
    "no longer possible",
    "guarantees that",
];

fn repo_root() -> PathBuf {
    // crates/core -> repo root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the repo root resolves")
}

fn markers() -> Vec<(&'static str, Value)> {
    [("docs", DOCS), ("agent", AGENT), ("clients", CLIENTS)]
        .into_iter()
        .map(|(name, raw)| {
            let v: Value = serde_json::from_str(raw).unwrap_or_else(|e| {
                panic!(
                    "CANNOT DETERMINE RANGE: the `{name}` catch-up marker does not parse ({e}). A \
                     pass that cannot read its marker must stop loudly here rather than default to \
                     rewriting everything or to rewriting nothing."
                )
            });
            (name, v)
        })
        .collect()
}

/// Paths listed in a marker's `covered` or `skipped`, as repo-relative strings.
fn listed(marker: &Value, key: &str) -> Vec<String> {
    marker[key]
        .as_array()
        .unwrap_or_else(|| panic!("marker has no `{key}` array"))
        .iter()
        .map(|e| {
            e["path"]
                .as_str()
                .unwrap_or_else(|| panic!("a `{key}` entry has no `path`: {e}"))
                .to_string()
        })
        .collect()
}

/// Walk a family's declared surfaces and return every file it covers, repo-relative, forward slashes.
fn surface_files(marker: &Value, root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for r in marker["surfaces"]["roots"].as_array().unwrap_or(&vec![]) {
        let base = r["path"].as_str().expect("a surface root has a path");
        let exts: Vec<String> = r["extensions"]
            .as_array()
            .expect("a surface root declares its extensions")
            .iter()
            .map(|e| e.as_str().unwrap().to_string())
            .collect();
        let recursive = r["recursive"].as_bool().unwrap_or(false);
        walk(&root.join(base), root, &exts, recursive, &mut out);
    }
    for f in marker["surfaces"]["files"].as_array().unwrap_or(&vec![]) {
        let p = f.as_str().unwrap();
        assert!(
            root.join(p).exists(),
            "a marker's declared surface file does not exist: {p}"
        );
        out.insert(p.to_string());
    }
    out
}

fn walk(dir: &Path, root: &Path, exts: &[String], recursive: bool, out: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            if recursive {
                walk(&path, root, exts, recursive, out);
            }
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // The marker never describes itself as a surface it must have rewritten.
        if name == "catchup-marker.json" {
            continue;
        }
        if exts.iter().any(|x| name.ends_with(x.as_str())) {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(rel);
        }
    }
}

/// True when `file` is the skipped entry itself or lives under it (a skipped directory covers its
/// whole subtree — a frozen archive is skipped as an archive, not as seventy separate decisions).
fn covered_by(entry: &str, file: &str) -> bool {
    file == entry || file.starts_with(&format!("{entry}/"))
}

#[test]
fn every_marker_can_scope_the_next_pass() {
    for (name, m) in markers() {
        let commit = m["anchor"]["commit"].as_str().unwrap_or_else(|| {
            panic!("CANNOT DETERMINE RANGE: `{name}` marker has no anchor commit")
        });
        assert!(
            commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit()),
            "`{name}` marker's anchor is not a full commit sha: {commit:?} — the next pass scans \
             anchor..now, and it cannot scan from a placeholder"
        );
        assert!(
            commit.chars().any(|c| c != '0'),
            "`{name}` marker's anchor is the zero commit"
        );
        let date = m["anchor"]["date"].as_str().unwrap_or_default();
        assert!(
            date.len() == 10 && date.starts_with("202"),
            "`{name}` marker's anchor has no ISO date: {date:?}"
        );
        assert!(
            !m["baseline_note"].as_str().unwrap_or_default().is_empty(),
            "`{name}` marker has no baseline note — the next operator has no way to judge whether \
             the world has shifted enough to scope wider than the range"
        );
        assert!(
            m["family"].as_str().is_some_and(|f| !f.is_empty()),
            "`{name}` marker names no family"
        );
    }
}

#[test]
fn the_denominator_holds_so_full_pass_is_a_predicate_not_a_claim() {
    let root = repo_root();
    for (name, m) in markers() {
        let files = surface_files(&m, &root);
        assert!(
            !files.is_empty(),
            "`{name}` marker's declared surfaces match no files — a marker that describes an empty \
             surface checks nothing, which is how a guard becomes a comparison of two empty sets"
        );
        let covered = listed(&m, "covered");
        let skipped = listed(&m, "skipped");

        let unlisted: Vec<&String> = files
            .iter()
            .filter(|f| {
                !covered.iter().any(|c| covered_by(c, f))
                    && !skipped.iter().any(|s| covered_by(s, f))
            })
            .collect();
        assert!(
            unlisted.is_empty(),
            "`{name}` catch-up marker lists neither covering nor skipping: {unlisted:?}\n\
             Every file under a family's surfaces must be in exactly one of `covered` / `skipped`. \
             Add it to one of them in the same change that added the file — otherwise a later reader \
             cannot tell 'this was current as of the anchor' from 'this was never in scope'."
        );

        let both: Vec<&String> = covered
            .iter()
            .filter(|c| skipped.iter().any(|s| s == *c))
            .collect();
        assert!(
            both.is_empty(),
            "`{name}` marker both covers and skips: {both:?}"
        );

        // A listed path that no longer exists is drift the marker is hiding.
        for p in covered.iter().chain(skipped.iter()) {
            assert!(
                root.join(p).exists(),
                "`{name}` marker lists `{p}`, which does not exist — a marker describing a deleted \
                 surface is a marker nobody has read since it was deleted"
            );
        }
    }
}

#[test]
fn skips_and_flags_are_first_class_not_footnotes() {
    for (name, m) in markers() {
        for e in m["skipped"].as_array().expect("a `skipped` array exists") {
            let p = e["path"].as_str().unwrap();
            let kind = e["kind"].as_str().unwrap_or("");
            assert!(
                matches!(kind, "frozen-archive" | "not-in-this-pass"),
                "`{name}` marker skips `{p}` with an unknown kind {kind:?} — 'deliberately never \
                 repaired' and 'this pass did not reach it' are different debts and must not read \
                 alike"
            );
            assert!(
                e["reason"].as_str().is_some_and(|r| r.len() > 10),
                "`{name}` marker skips `{p}` with no reason"
            );
        }
        // Cross-boundary obligations the per-change check cannot gate accumulate here, and each says
        // what would drain it — otherwise the queue is a list of worries rather than a list of work.
        for e in m["flagged"].as_array().expect("a `flagged` array exists") {
            let p = e["path"].as_str().unwrap_or("<none>");
            assert!(
                e["flagged_on"].as_str().is_some_and(|d| d.len() == 10),
                "`{name}` marker flags `{p}` with no date"
            );
            assert!(
                e["what"].as_str().is_some_and(|w| w.len() > 20),
                "`{name}` marker flags `{p}` with no description"
            );
            assert!(
                e["drained_when"].as_str().is_some_and(|w| !w.is_empty()),
                "`{name}` marker flags `{p}` with no exit condition — a flag that cannot be drained \
                 is a permanent worry, not an obligation"
            );
        }
    }
}

#[test]
fn a_marker_records_what_was_done_never_what_is_hoped() {
    for (name, m) in markers() {
        let mut prose = m["baseline_note"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase();
        for e in m["covered"].as_array().unwrap_or(&vec![]) {
            prose.push_str(&e["note"].as_str().unwrap_or_default().to_lowercase());
        }
        for phrase in PREDICTIONS {
            assert!(
                !prose.contains(phrase),
                "`{name}` marker predicts rather than records: it contains {phrase:?}.\n\
                 This technique exists because of a marker that ended 'the per-session hook now \
                 prevents this kind of drift; bulk rewrites should not be needed again' — written on \
                 the day the never-fired hook landed. The next operator trusted it and scoped narrow; \
                 the truth was zero enforcement and fifteen months of drift. State what this pass did \
                 and against what. When the mechanisms around a marker change, it gains a dated note \
                 THAT they changed, never an assertion that they work."
            );
        }
    }
}

/// The guard's own seeded-failure proof: the parsers must actually find something, and the
/// denominator check must actually fail on an unlisted file — otherwise a future refactor that makes
/// either return an empty set turns these tests into tautologies.
#[test]
fn the_guard_can_go_red() {
    let root = repo_root();
    let (_, docs) = markers().into_iter().find(|(n, _)| *n == "docs").unwrap();
    let files = surface_files(&docs, &root);
    assert!(
        files.len() > 10,
        "the docs surface walk found only {} files — the walker lost the tree, not the repo",
        files.len()
    );
    assert!(files.contains("docs/ARCHITECTURE.md"));
    // The archive is inside the surface (so it is a real denominator member) and is skipped as a
    // directory (so it is one decision, not seventy).
    assert!(files
        .iter()
        .any(|f| f.starts_with("docs/harness/perf-feature-2026-07-16/")));
    assert!(covered_by(
        "docs/harness/perf-feature-2026-07-16",
        "docs/harness/perf-feature-2026-07-16/INDEX.md"
    ));
    assert!(!covered_by("docs/ALERTS.md", "docs/ALERTS_OTHER.md"));

    // A file in neither list is exactly what the denominator test must catch.
    let covered = listed(&docs, "covered");
    let skipped = listed(&docs, "skipped");
    let invented = "docs/A_NEW_DOC_NOBODY_DISPOSITIONED.md";
    assert!(!covered.iter().any(|c| covered_by(c, invented)));
    assert!(!skipped.iter().any(|s| covered_by(s, invented)));

    // And the prediction check must actually match when a prediction is present.
    let hopeful = "the per-session hook now prevents this kind of drift; bulk rewrites should not be needed again";
    assert!(PREDICTIONS.iter().any(|p| hopeful.contains(p)));
}
