//! Structural guard for the fixed-width timestamp invariant (see `codec::fmt_ts`): every RFC3339
//! formatting call in the workspace must use `(SecondsFormat::Nanos, true)`. A few crates that
//! don't link the store re-implement the format inline (runner); this guard is what
//! keeps those copies from drifting to a variable-width form that would break string range filters,
//! ORDER BY, and keyset cursors. Pointer: Obsidian vault → Architect/strong-patterns.md.

use std::fs;
use std::path::Path;

// Assembled at runtime so this file's own source doesn't trip the scan.
fn needle() -> String {
    format!("to_rfc3339{}", "_opts")
}

const REQUIRED_ARGS: &str = "SecondsFormat::Nanos, true";

fn scan(dir: &Path, needle: &str, violations: &mut Vec<String>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "target" && name != ".git" {
                scan(&path, needle, violations);
            }
            continue;
        }
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for (start, _) in content.match_indices(needle) {
            // The args live within a short window after the call; a compliant call always fits.
            let window = &content[start..content.len().min(start + 120)];
            if !window.contains(REQUIRED_ARGS) {
                let line = content[..start].lines().count();
                violations.push(format!("{}:{line}", path.display()));
            }
        }
    }
}

#[test]
fn all_rfc3339_formatting_is_fixed_width_nanos_utc() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let needle = needle();
    let mut violations = Vec::new();
    for dir in ["crates", "clients"] {
        scan(&root.join(dir), &needle, &mut violations);
    }
    assert!(
        violations.is_empty(),
        "timestamp formatted without (SecondsFormat::Nanos, true) — variable-width timestamps \
         break string range filters / ORDER BY / keyset cursors on every store backend. Use \
         lighttrack_store::codec::fmt_ts (or match its exact format). See \
         Architect/strong-patterns.md (fixed-width RFC3339 codec seam). Offenders:\n{}",
        violations.join("\n")
    );
}

// The guard above only sees the `_opts` form. A bare call to the plain formatter is the same
// defect with no arguments to check: it emits `+00:00` and a fractional width that follows the
// value, so a string produced by it and compared against a stored timestamp is wrong exactly
// when the sub-second digits end in zeros.
fn bare_needle() -> String {
    format!(".to_rfc3339{}", "()")
}

/// Production files that format a timestamp for a human, a header or a wire body that is parsed
/// back to an instant before it reaches the store. None of them compares the string against a
/// stored column. Adding a path here is a claim that the new call site is the same kind; a call
/// that feeds a range filter, an ORDER BY or a cursor must use `codec::fmt_ts` instead.
const BARE_ALLOWED: &[(&str, &str)] = &[
    (
        "crates/api/src/collective/contribute.rs",
        "message text for the operator",
    ),
    (
        "crates/api/src/events_query.rs",
        "400 message and a response header",
    ),
    (
        "crates/api/src/redaction.rs",
        "echo of the query window in a response body",
    ),
    (
        "crates/gateway/src/telemetry.rs",
        "event body; parsed to an instant, stored via fmt_ts",
    ),
    ("crates/responder/src/ledger.rs", "outbound resolution note"),
];

fn scan_bare(root: &Path, dir: &Path, needle: &str, violations: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if name != "target" && name != ".git" && name != "tests" {
                scan_bare(root, &path, needle, violations);
            }
            continue;
        }
        if path.extension().is_none_or(|e| e != "rs") || name.starts_with("tests_") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let prod = content.split("#[cfg(test)]").next().unwrap_or("");
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if prod.contains(needle) && !BARE_ALLOWED.iter().any(|(p, _)| rel.ends_with(p)) {
            violations.push(rel);
        }
    }
}

#[test]
fn production_code_does_not_format_bare_rfc3339() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let needle = bare_needle();
    let mut violations = Vec::new();
    scan_bare(&root, &root.join("crates"), &needle, &mut violations);
    assert!(
        violations.is_empty(),
        "bare RFC3339 formatting in production code: it emits `+00:00` and a value-dependent \
         fractional width, so a string built here does not sort against stored timestamps. Use \
         lighttrack_store::codec::fmt_ts, or, if the string only ever reaches a human or a parsed \
         wire body, add the file to BARE_ALLOWED with the reason. Offenders:\n{}",
        violations.join("\n")
    );
}

#[test]
fn bare_allowlist_has_no_stale_entries() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let needle = bare_needle();
    for (p, _) in BARE_ALLOWED {
        let content = fs::read_to_string(root.join(p)).unwrap_or_default();
        let prod = content.split("#[cfg(test)]").next().unwrap_or("");
        assert!(
            prod.contains(&needle),
            "BARE_ALLOWED entry {p} no longer formats a bare timestamp; remove it"
        );
    }
}
