//! The tool allowlist: what each mode always gets, and which specs count as read-only.
//!
//! The direction of the default matters more than the list does. A tool spec we do not recognise is
//! treated as **write-capable**, so a new or misspelled tool is rejected from a read-only scan
//! rather than quietly admitted — the failure mode of a too-strict allowlist is a build error, and
//! of a too-lenient one, an edited repository.

use super::{Invocation, Mode};

/// The tools a [`Mode::ReadonlyScan`] always gets. Anything else must be named by the caller and
/// must pass [`is_readonly_tool`].
pub const READONLY_BASE_TOOLS: &[&str] = &["Read", "Glob", "Grep", "LS"];

/// Read-only `Bash(...)` prefixes. The check is a prefix match on the command inside the
/// parentheses, so `Bash(git log:*)` is fine and `Bash(git push:*)` is not.
const READONLY_BASH_PREFIXES: &[&str] = &[
    "git log",
    "git diff",
    "git show",
    "git status",
    "git blame",
    "git ls-files",
    "ls",
    "cat",
    "head",
    "tail",
    "wc",
    "rg",
    "grep",
    "find",
];

/// Non-`Bash` tools that read but never write.
const READONLY_TOOL_NAMES: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "LS",
    "NotebookRead",
    "WebFetch",
    "WebSearch",
];

/// Whether a tool spec can only read.
pub(crate) fn is_readonly_tool(tool: &str) -> bool {
    if let Some(rest) = tool.strip_prefix("Bash(") {
        let inner = rest.trim_end_matches(')').trim_end_matches(":*").trim();
        return READONLY_BASH_PREFIXES.iter().any(|p| inner.starts_with(p));
    }
    READONLY_TOOL_NAMES.contains(&tool)
}

/// The final allowlist for this invocation: the mode's base set plus the caller's extras,
/// deduplicated and order-stable so the argv is comparable across runs (and testable).
pub(crate) fn allowlist(inv: &Invocation<'_>) -> Vec<String> {
    if inv.mode == Mode::Generate {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    if inv.mode == Mode::ReadonlyScan {
        out.extend(READONLY_BASE_TOOLS.iter().map(|t| t.to_string()));
    }
    for t in &inv.allowed_tools {
        if !out.iter().any(|x| x == t) {
            out.push(t.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_recognised_read_only_specs_pass() {
        for good in [
            "Read",
            "Glob",
            "Grep",
            "LS",
            "WebFetch",
            "Bash(git log:*)",
            "Bash(git diff --stat)",
        ] {
            assert!(is_readonly_tool(good), "{good} should be read-only");
        }
        for bad in [
            "Write",
            "Edit",
            "Task",
            "NotebookEdit",
            "Bash(git push:*)",
            "Bash(rm -rf:*)",
            "Bash(npm install:*)",
            "Reader", // near-miss: not the tool, not admitted
        ] {
            assert!(!is_readonly_tool(bad), "{bad} should not be read-only");
        }
    }

    #[test]
    fn the_base_set_is_added_once_and_extras_keep_their_order() {
        let inv = Invocation::readonly_scan("x", "m")
            .with_cwd("/repo")
            .with_allowed_tools(["Read", "Bash(git log:*)", "WebFetch"]);
        assert_eq!(
            allowlist(&inv),
            vec!["Read", "Glob", "Grep", "LS", "Bash(git log:*)", "WebFetch"]
        );
        // A generate run gets none, even if one were somehow set.
        assert!(allowlist(&Invocation::generate("x", "m")).is_empty());
        // An edit run gets no base set — its permission mode is the gate, not an allowlist.
        let edit = Invocation::edit("x", "m")
            .with_cwd("/repo")
            .with_permission_mode(Some("acceptEdits"))
            .with_allowed_tools(["Write"]);
        assert_eq!(allowlist(&edit), vec!["Write"]);
    }
}
