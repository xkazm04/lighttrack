//! Hold the agent-guidance documents to `.ai/manifest.yaml`'s `guidance:` declaration.
//!
//! This repository ships more than one file an agent might open first, and for a while they
//! disagreed: the root `CLAUDE.md` is a Rust working agreement, while `.claude/CLAUDE.md` still
//! carries the generator's scaffold telling a reader to run `npm run build` on a workspace that has
//! no npm build. "How do I build this" was answered by whichever file was opened first — and the
//! answer that loses is the one nobody knows they lost.
//!
//! The manifest already names the winner (`guidance.canonical`) and lists the losers
//! (`guidance.projections`), with the ones known to contradict it quarantined in
//! `guidance.staleProjections`. This test is what makes those three lists mean something:
//!
//! * the canonical document exists and hands commands off to the manifest, so there is exactly one
//!   place a command is written down (`capabilities:`, held to CI by `manifest_guard.rs` and
//!   `blocking_gate_guard.rs`);
//! * every projection exists — a pointer to a missing file is worse than no pointer;
//! * a projection that is **not** declared stale is a POINTER: it names the canonical file and
//!   carries no command line. This is the rule that stops the next generator-written or
//!   agent-written guidance file from becoming a second, quieter answer;
//! * every projection that **is** declared stale is named in the canonical document, so a reader who
//!   opens the good file first is told which document to disbelieve.
//!
//! Removing a file from `staleProjections` therefore does not close the gap on its own: the pointer
//! rules above start applying to it in the same commit, and the build stays red until its body is
//! actually a pointer.
//!
//! The YAML is parsed by hand, as everywhere else in this crate: `lighttrack-core` is deliberately
//! dependency-light (deny.toml's bans policy) and the shape needed here is two lines.

use std::path::{Path, PathBuf};

const MANIFEST: &str = include_str!("../../../.ai/manifest.yaml");

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/core/../.. is the repository root")
        .to_path_buf()
}

/// A scalar or flow-sequence value nested one level under `guidance:`.
fn guidance_value(yaml: &str, key: &str) -> Option<String> {
    let mut inside = false;
    for line in yaml.lines() {
        if line.starts_with("guidance:") {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if !line.starts_with(' ') && !line.trim().is_empty() {
            break; // a new top-level key ends the block
        }
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if let Some(v) = trimmed.strip_prefix(&format!("{key}:")) {
            return Some(v.trim().to_string());
        }
    }
    None
}

fn flow_list(v: &str) -> Vec<String> {
    v.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Lines that read as an instruction to run something — a shell command at the start of a line,
/// inside a fenced block or not. A pointer document has none of these; a document that answers
/// "how do I build this" has nothing but.
fn command_lines(md: &str) -> Vec<String> {
    fn is_command(line: &str) -> bool {
        let first = line.split_whitespace().next().unwrap_or("");
        matches!(first, "npm" | "npx" | "pnpm" | "yarn" | "cargo" | "make" | "just" | "sh")
    }
    md.lines()
        .map(|l| l.trim().trim_start_matches("$ ").trim_start())
        .filter(|l| is_command(l))
        .map(str::to_string)
        .collect()
}

fn canonical() -> String {
    guidance_value(MANIFEST, "canonical").expect("guidance.canonical must be declared")
}

#[test]
fn the_canonical_guidance_exists_and_defers_commands_to_the_manifest() {
    let canonical = canonical();
    let path = repo_root().join(&canonical);
    assert!(
        path.is_file(),
        "guidance.canonical names {canonical}, which does not exist in a fresh clone"
    );
    let text = std::fs::read_to_string(&path).expect("read the canonical guidance");
    let commands_from =
        guidance_value(MANIFEST, "commandsFrom").expect("guidance.commandsFrom must be declared");
    assert!(
        text.contains(&commands_from),
        "{canonical} must hand commands off to {commands_from} — one place, or they drift"
    );
}

#[test]
fn every_projection_exists_and_the_live_ones_are_pointers() {
    let canonical = canonical();
    let projections = flow_list(&guidance_value(MANIFEST, "projections").unwrap_or_default());
    let stale = flow_list(&guidance_value(MANIFEST, "staleProjections").unwrap_or_default());
    assert!(
        !projections.is_empty(),
        "guidance.projections did not parse — this test would then assert nothing"
    );

    for p in &projections {
        let path = repo_root().join(p);
        assert!(path.is_file(), "projection {p} does not exist in a fresh clone");
        if stale.contains(p) {
            continue; // quarantined, and asserted about below
        }
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {p}: {e}"));
        // A pointer names what it points at. Compared on the file NAME, not the path, because a
        // projection in a subdirectory links to it relatively (`../CLAUDE.md`).
        let name = Path::new(&canonical)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&canonical);
        assert!(
            text.contains(name),
            "{p} is a declared projection of {canonical} but never names it — an agent opening \
             this file first has no way back to the canonical answer"
        );
        let commands = command_lines(&text);
        assert!(
            commands.is_empty(),
            "{p} is a projection, not an answer: it carries command line(s) {commands:?}.\n\
             Commands are written down exactly once, in `capabilities:` in .ai/manifest.yaml \
             (held to CI by blocking_gate_guard.rs and to scripts/gates.sh by manifest_guard.rs). \
             A command in prose is a command that goes stale silently."
        );
    }

    for s in &stale {
        assert!(
            projections.contains(s),
            "guidance.staleProjections names {s}, which is not a declared projection"
        );
    }
}

#[test]
fn a_stale_projection_is_named_in_the_document_that_supersedes_it() {
    // The one thing that can be done about a contradiction you cannot delete: make sure the reader
    // who opened the RIGHT file is told the wrong one exists. (`.claude/CLAUDE.md` has outlasted two
    // attempts to rewrite it — the agent harness refuses writes under `.claude/`, so it needs a
    // human. Until then, this is the mitigation, and it is checked rather than hoped for.)
    let canonical = canonical();
    let text = std::fs::read_to_string(repo_root().join(&canonical)).expect("read the canonical");
    for s in flow_list(&guidance_value(MANIFEST, "staleProjections").unwrap_or_default()) {
        assert!(
            text.contains(&s) || text.contains(&s.replace('/', "\\")),
            "{s} is declared stale but {canonical} never warns about it — a reader who opens the \
             canonical file first still has no reason to disbelieve {s}"
        );
    }
}

#[test]
fn the_parsers_can_go_red() {
    // A guard whose parser silently returns nothing passes on an empty repository and proves it.
    let yaml = "guidance:\n  canonical: A.md\n  projections: [b.md, c/d.md]\n\nnext:\n  x: 1\n";
    assert_eq!(guidance_value(yaml, "canonical").as_deref(), Some("A.md"));
    let projections = flow_list(&guidance_value(yaml, "projections").unwrap());
    assert_eq!(projections, ["b.md", "c/d.md"]);
    assert_eq!(guidance_value(yaml, "staleProjections"), None);
    assert!(guidance_value(yaml, "x").is_none(), "the block ends at next:");

    // The scaffold this whole file exists because of, in both the shapes it appears in.
    let fenced = command_lines("run it:\n```bash\nnpm run build\n```\n");
    assert_eq!(fenced, ["npm run build"]);
    assert_eq!(command_lines("- `cargo build -p x`"), Vec::<String>::new());
    assert!(command_lines("see `npm run build` for why not").is_empty());
    assert_eq!(command_lines("$ cargo test --workspace"), ["cargo test --workspace"]);
}
