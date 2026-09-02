//! The single `claude` executable resolver.
//!
//! On Windows a global npm install puts only `claude.cmd` / `claude.ps1` on PATH, and a child
//! process cannot invoke those shims with quote-heavy args — so we prefer the real `claude.exe`
//! they wrap. Two copies of this logic used to exist (engine + responder) and they had already
//! drifted: only one of them knew about the native installer under `~/.local/bin`. One copy now.

/// Resolve a runnable claude executable. An explicit path is honoured verbatim; the bare name
/// `claude` is resolved to a real executable where we can find one.
pub fn resolve_claude_bin(given: &str) -> String {
    if given != "claude" {
        return given.to_string();
    }
    #[cfg(windows)]
    {
        use std::path::Path;
        // Native installer first (it is the current install path), then a global npm install.
        if let Ok(home) = std::env::var("USERPROFILE") {
            let p = format!("{home}\\.local\\bin\\claude.exe");
            if Path::new(&p).exists() {
                return p;
            }
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            let p = format!(
                "{appdata}\\npm\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.exe"
            );
            if Path::new(&p).exists() {
                return p;
            }
        }
    }
    given.to_string()
}

#[cfg(test)]
mod tests {
    use super::resolve_claude_bin;

    #[test]
    fn explicit_paths_pass_through_untouched() {
        assert_eq!(resolve_claude_bin("/usr/bin/claude"), "/usr/bin/claude");
        assert_eq!(
            resolve_claude_bin("C:\\tools\\claude.exe"),
            "C:\\tools\\claude.exe"
        );
    }

    #[test]
    fn the_bare_name_resolves_to_something_runnable() {
        // Either a real .exe was found, or we fall back to the name and let PATH decide.
        let got = resolve_claude_bin("claude");
        assert!(got == "claude" || got.ends_with("claude.exe"), "{got}");
    }
}
