//! Structural guard for the fixed-width timestamp invariant (see `codec::fmt_ts`): every RFC3339
//! formatting call in the workspace must use `(SecondsFormat::Nanos, true)`. A few crates that
//! don't link the store re-implement the format inline (runner, api rejections); this guard is what
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
