//! A checker for `.ai/manifest.yaml` against the contract that ships beside it.
//!
//! `.ai/ai-manifest.spec.md` §10 lists what a conformant checker asserts. This is **a** checker, not
//! the definition — the spec's reimplementation clause is the point: every rule is written down
//! precisely enough that a second implementation could be built from the document alone, and this
//! one exists to prove the document is not decorative.
//!
//! The three rules with real teeth:
//!
//! * **C2** — every `paths:` pointer resolves. Pointers-not-embeds is only a good trade while the
//!   pointers work; a broken one turns "look here" into a dead end for exactly the offline reader
//!   the manifest exists to serve.
//! * **C3** — every `controls:` entry names a declared capability. `controls` is a projection of the
//!   CI workflow, and a projection nobody checks is a claim.
//! * **C1** — every capability has a command and an explicit `verified` claim.
//!
//! It parses the two blocks it needs by hand rather than adding a YAML dependency: `lighttrack-core`
//! is deliberately dependency-light (see deny.toml's bans policy), and the shape needed here —
//! two-space-indented keys with scalar or inline-flow values — is a dozen lines. A parser this small
//! is also part of the point: nothing about this contract should require a library to read.

use std::collections::BTreeMap;
use std::path::Path;

const MANIFEST: &str = include_str!("../../../.ai/manifest.yaml");

/// The repository root, relative to this test file.
fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/core/../.. is the repository root")
}

/// Strip a trailing `# comment` from a scalar value, respecting quotes.
fn strip_comment(v: &str) -> &str {
    let bytes = v.as_bytes();
    let mut quote: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match (quote, b) {
            (None, b'"') | (None, b'\'') => quote = Some(b),
            (Some(q), c) if c == q => quote = None,
            (None, b'#') if i > 0 && bytes[i - 1] == b' ' => return v[..i].trim_end(),
            _ => {}
        }
    }
    v
}

/// The `key: value` pairs nested exactly one level under `section`, as raw value text.
///
/// Enough for `capabilities`, `paths` and `controls`, whose values are scalars or flow
/// mappings/sequences. A value written across several lines (a formatter does that to
/// `controls.ciHardPass`) is folded onto its key: everything indented deeper than the key, joined
/// with single spaces, is that key's value.
fn block(yaml: &str, section: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut inside = false;
    let mut current: Option<String> = None;
    for line in yaml.lines() {
        if line.starts_with(&format!("{section}:")) {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if !line.starts_with(' ') && !line.trim().is_empty() {
            break; // a new top-level key ends the section
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - trimmed.len();
        if indent > 2 {
            // A continuation of the key above it.
            if let Some(key) = current.as_ref() {
                let v = out.entry(key.clone()).or_default();
                if !v.is_empty() {
                    v.push(' ');
                }
                v.push_str(strip_comment(trimmed).trim());
            }
            continue;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_string();
            out.insert(key.clone(), strip_comment(v.trim()).to_string());
            current = Some(key);
        }
    }
    out
}

/// The items of a one-line flow sequence `[a, b, c]`.
fn flow_list(v: &str) -> Vec<String> {
    v.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[test]
fn the_manifest_declares_its_identity_and_version() {
    // §10.2 and §10.3. `schema` is the identity a reader keys off; `schemaVersion` is what it uses
    // to decide whether it can act.
    assert!(
        MANIFEST.contains("schema: ai-manifest"),
        "the manifest must identify itself as an ai-manifest"
    );
    let version = MANIFEST
        .lines()
        .find_map(|l| l.strip_prefix("schemaVersion:"))
        .map(|v| v.trim().trim_matches('"').to_string())
        .expect("schemaVersion is required");
    assert_eq!(
        version.split('.').count(),
        3,
        "schemaVersion must be semver, got {version:?}"
    );
}

#[test]
fn the_spec_the_manifest_names_resolves_from_this_clone() {
    // The deviation this whole file answers: the manifest used to point at "the ai-manifest spec
    // carried by the registry consumer lane", which resolved from no clone and in fact named no
    // document that existed anywhere. A contract whose definition lives somewhere else stops meaning
    // anything the moment that somewhere else is unreachable — and the moment a manifest matters
    // most is exactly the offline one.
    let spec = repo_root().join(".ai/ai-manifest.spec.md");
    assert!(
        spec.is_file(),
        "the shipped contract must exist at {}",
        spec.display()
    );
    assert!(
        MANIFEST.contains("ai-manifest.spec.md"),
        "and the manifest must point at it by path, so a reader can follow the pointer"
    );
    let text = std::fs::read_to_string(&spec).expect("read the spec");
    assert!(
        text.contains("Reimplementation clause"),
        "a spec sufficient to reimplement from says so, and is held to it"
    );
    for rule in ["C1", "C2", "C3"] {
        assert!(text.contains(rule), "the spec must define rule {rule}");
    }
}

#[test]
fn every_capability_carries_a_command_and_an_explicit_verified_claim() {
    // C1. `verified` is a claim about evidence — somebody ran it and watched it pass — so it is
    // required rather than defaulted: an absent field would read as "false" and as "nobody wrote
    // one" at the same time.
    let caps = block(MANIFEST, "capabilities");
    assert!(caps.len() >= 5, "parsed only {} capabilities", caps.len());
    for (name, body) in &caps {
        assert!(
            body.contains("command:"),
            "capability '{name}' has no command: {body}"
        );
        assert!(
            body.contains("verified: true") || body.contains("verified: false"),
            "capability '{name}' must state `verified` explicitly: {body}"
        );
        let cmd = body
            .split("command:")
            .nth(1)
            .and_then(|s| s.split(',').next())
            .unwrap_or_default();
        assert!(
            cmd.trim().len() > 2,
            "capability '{name}' has an empty command"
        );
    }
}

#[test]
fn every_path_pointer_resolves() {
    // C2. Pointers-not-embeds is a good trade only while the pointers work.
    let paths = block(MANIFEST, "paths");
    assert!(!paths.is_empty(), "the paths block did not parse");
    for (slot, target) in &paths {
        let target = target.trim_matches('"');
        let full = repo_root().join(target);
        assert!(
            full.exists(),
            "paths.{slot} points at {target}, which does not exist in a fresh clone"
        );
    }
}

#[test]
fn every_control_names_a_declared_capability() {
    // C3. `controls` is a projection of .github/workflows/ci.yml. A projection nobody checks is a
    // claim — and this one is how an agent decides which command's failure actually blocks a merge.
    let caps = block(MANIFEST, "capabilities");
    let controls = block(MANIFEST, "controls");
    assert!(
        controls.contains_key("ciHardPass"),
        "controls must declare what blocks: {controls:?}"
    );
    let mut checked = 0;
    for (lane, list) in &controls {
        for name in flow_list(list) {
            assert!(
                caps.contains_key(&name),
                "controls.{lane} names '{name}', which is not a declared capability"
            );
            checked += 1;
        }
    }
    assert!(checked >= 5, "only {checked} control entries parsed");
}

#[test]
fn the_parsers_can_go_red() {
    // The guard's own seeded-failure proof: a checker whose parser silently returns nothing is a
    // test that passes on an empty manifest and proves exactly that.
    let bad = "capabilities:\n  test: { command: \"x\", verified: true }\ncontrols:\n  ciHardPass: [test, missing]\n";
    let caps = block(bad, "capabilities");
    let controls = block(bad, "controls");
    assert_eq!(caps.len(), 1);
    assert_eq!(flow_list(&controls["ciHardPass"]), ["test", "missing"]);
    assert!(
        !caps.contains_key("missing"),
        "this is the drift the real test catches"
    );

    // Multi-line flow sequences (how `controls` is actually formatted) fold onto their key.
    let wrapped = "controls:\n  ciHardPass:\n    [\n      a,\n      b,\n    ]\n";
    assert_eq!(
        flow_list(&block(wrapped, "controls")["ciHardPass"]),
        ["a", "b"]
    );

    // A trailing comment is not part of a path.
    assert_eq!(strip_comment("docs/ # the docs tree"), "docs/");
    assert_eq!(strip_comment("\"a # b\""), "\"a # b\"");
}
