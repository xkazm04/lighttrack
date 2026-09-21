//! Finding a runnable `codex`.
//!
//! On Windows an npm install puts only a Node shim on PATH, and what it wraps is a native
//! `codex.exe` several directories down. The Claude resolver hardcodes the one npm location it
//! knows, and that approach is **wrong here**: this machine had two Codex installs, the one PATH
//! resolves (0.154.0, current) and a stale global one under `%APPDATA%\npm` (0.139.0) — and the
//! stale one refuses newer models outright ("requires a newer version of Codex"). A fixed path
//! would have benchmarked a matrix on whichever install it happened to name.
//!
//! So the search follows **PATH order**, which is exactly the install the operator's own shell would
//! run, and only then falls back to the default npm prefix.

use std::path::{Path, PathBuf};

/// The native binary inside an npm global prefix, per architecture.
const NATIVE: [&str; 2] = [
    "node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe",
    "node_modules/@openai/codex/node_modules/@openai/codex-win32-arm64/vendor/aarch64-pc-windows-msvc/bin/codex.exe",
];

/// Resolve a runnable codex executable. `LIGHTTRACK_CODEX_BIN` is honoured verbatim; otherwise the
/// native binary behind the first npm install on PATH; otherwise the bare name.
pub fn resolve_codex_bin() -> String {
    if let Some(explicit) = std::env::var("LIGHTTRACK_CODEX_BIN")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        return explicit;
    }
    if !cfg!(windows) {
        // Off Windows the binary on PATH is directly runnable, and PATH already decides which.
        return "codex".to_string();
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(appdata).join("npm"));
    }
    first_native(&dirs)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "codex".to_string())
}

/// The native binary of the first directory, in order, that holds an npm Codex install.
fn first_native(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().find_map(|d| native_in(d))
}

fn native_in(dir: &Path) -> Option<PathBuf> {
    NATIVE.iter().map(|rel| dir.join(rel)).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_prefix(name: &str, with_binary: bool) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lt-codex-resolve-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if with_binary {
            let exe = dir.join(NATIVE[0]);
            std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
            std::fs::write(&exe, b"").unwrap();
        }
        dir
    }

    /// **The regression this module exists for.** Two installs: the one earlier on PATH wins, which
    /// is the one the operator's shell runs — never whichever a hardcoded path happens to name.
    #[test]
    fn the_install_earlier_on_path_wins() {
        let current = fake_prefix("current", true);
        let stale = fake_prefix("stale", true);
        let empty = fake_prefix("empty", false);
        let got = first_native(&[empty.clone(), current.clone(), stale.clone()]).unwrap();
        assert!(got.starts_with(&current), "{}", got.display());
        let got = first_native(&[stale.clone(), current.clone()]).unwrap();
        assert!(got.starts_with(&stale), "order is the whole rule");
        assert!(first_native(std::slice::from_ref(&empty)).is_none());
        for d in [current, stale, empty] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn the_resolver_always_returns_something_runnable_or_the_bare_name() {
        let got = resolve_codex_bin();
        assert!(
            got == "codex"
                || got.ends_with("codex.exe")
                || std::env::var("LIGHTTRACK_CODEX_BIN").is_ok(),
            "{got}"
        );
    }
}
