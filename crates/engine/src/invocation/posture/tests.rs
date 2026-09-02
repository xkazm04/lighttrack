//! The posture matrix, in one place: what each mode's argv looks like, which contradictions are
//! refused before a child exists, and the billing-key decision.

use std::path::PathBuf;
use std::process::Command;

use super::{apply_auth, plan};
use crate::invocation::tools::{allowlist, is_readonly_tool};
use crate::invocation::Invocation;
use crate::EngineError;

fn args_of(inv: &Invocation<'_>) -> Vec<String> {
    plan(inv).expect("posture should accept").args
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).map(String::as_str)
}

/// The posture matrix: one assertion block per mode over the argv it produces.
#[test]
fn posture_matrix_pins_each_mode_argv() {
    // Generate: no tools, no permission mode, a neutral cwd, prompt never on argv.
    let inv = Invocation::generate("judge this", "opus@xhigh");
    let p = plan(&inv).unwrap();
    assert_eq!(flag(&p.args, "--model"), Some("opus"));
    assert_eq!(flag(&p.args, "--effort"), Some("xhigh"));
    assert_eq!(flag(&p.args, "--output-format"), Some("json"));
    assert!(!p.args.iter().any(|a| a == "--allowedTools"));
    assert!(!p.args.iter().any(|a| a == "--permission-mode"));
    assert!(!p.args.iter().any(|a| a == "judge this"));
    assert!(p.strict_exit);
    assert_ne!(p.cwd, std::env::current_dir().unwrap());

    // ReadonlyScan: base allowlist + extras, last, and a controlled non-zero exit is readable.
    let inv = Invocation::readonly_scan("look", "sonnet")
        .with_cwd("/repo")
        .with_allowed_tools(["Bash(git log:*)"])
        .with_permission_mode(Some("default"))
        .with_budget_usd(Some(1.0));
    let p = plan(&inv).unwrap();
    let at = p.args.iter().position(|a| a == "--allowedTools").unwrap();
    assert_eq!(
        &p.args[at + 1..],
        &["Read", "Glob", "Grep", "LS", "Bash(git log:*)"]
    );
    assert_eq!(at + 6, p.args.len(), "--allowedTools must be last");
    assert_eq!(flag(&p.args, "--permission-mode"), Some("default"));
    assert_eq!(flag(&p.args, "--max-budget-usd"), Some("1.00"));
    assert!(!p.strict_exit);

    // Edit: workspace + permission mode carried through; no base allowlist forced on it.
    let inv = Invocation::edit("fix", "sonnet")
        .with_cwd("/repo")
        .with_permission_mode(Some("acceptEdits"));
    let p = plan(&inv).unwrap();
    assert_eq!(flag(&p.args, "--permission-mode"), Some("acceptEdits"));
    assert_eq!(p.cwd, PathBuf::from("/repo"));
    assert!(!p.args.iter().any(|a| a == "--allowedTools"));
}

/// A read-only scan that lists a write-capable tool is refused, whatever the tool looks like.
#[test]
fn readonly_scan_never_carries_a_write_tool() {
    for bad in [
        "Write",
        "Edit",
        "Bash(git push:*)",
        "Bash(rm -rf:*)",
        "Task",
    ] {
        let inv = Invocation::readonly_scan("x", "sonnet")
            .with_cwd("/repo")
            .with_allowed_tools([bad]);
        let err = plan(&inv).expect_err("write tool must be rejected");
        assert!(matches!(err, EngineError::Posture(_)), "{bad}: {err:?}");
    }
    // …and every tool a legal scan actually emits is read-only, base set included.
    let inv = Invocation::readonly_scan("x", "sonnet")
        .with_cwd("/repo")
        .with_allowed_tools(["Bash(git diff:*)", "WebFetch"]);
    for tool in allowlist(&inv) {
        assert!(is_readonly_tool(&tool), "{tool} is not read-only");
    }
}

#[test]
fn contradictions_fail_before_spawning() {
    let cases: Vec<(&str, Invocation<'_>)> = vec![
        (
            "generate + tools",
            Invocation::generate("x", "m").with_allowed_tools(["Read"]),
        ),
        (
            "generate + cwd",
            Invocation::generate("x", "m").with_cwd("/repo"),
        ),
        (
            "generate + permission mode",
            Invocation::generate("x", "m").with_permission_mode(Some("plan")),
        ),
        ("edit without cwd", Invocation::edit("x", "m")),
        (
            "edit without permission mode",
            Invocation::edit("x", "m").with_cwd("/repo"),
        ),
        ("scan without cwd", Invocation::readonly_scan("x", "m")),
        (
            "scan in acceptEdits",
            Invocation::readonly_scan("x", "m")
                .with_cwd("/repo")
                .with_permission_mode(Some("acceptEdits")),
        ),
        (
            "bad effort",
            Invocation::generate("x", "m").with_effort(Some("turbo")),
        ),
        (
            "non-positive budget",
            Invocation::edit("x", "m")
                .with_cwd("/repo")
                .with_permission_mode(Some("acceptEdits"))
                .with_budget_usd(Some(0.0)),
        ),
    ];
    for (name, inv) in cases {
        match plan(&inv) {
            Err(EngineError::Posture(_)) => {}
            other => panic!("{name}: expected a posture error, got {other:?}"),
        }
    }
}

#[test]
fn schema_system_and_bare_reach_the_argv() {
    let inv = Invocation::generate("x", "haiku")
        .with_system(Some("be terse"))
        .with_schema(Some("{\"type\":\"object\"}"))
        .with_bare(true);
    let args = args_of(&inv);
    assert_eq!(flag(&args, "--append-system-prompt"), Some("be terse"));
    assert_eq!(flag(&args, "--json-schema"), Some("{\"type\":\"object\"}"));
    assert!(args.iter().any(|a| a == "--bare"));
}

#[test]
fn seat_runs_strip_the_billing_key_and_bare_runs_demand_it() {
    // The env var is process-global; drive both branches off an explicit `bare` flag and a
    // key we set/clear here rather than whatever the developer's shell happens to hold.
    let mut cmd = Command::new("echo");
    std::env::set_var("ANTHROPIC_API_KEY", "sk-test");
    assert!(apply_auth(&mut cmd, false).is_ok());
    assert!(apply_auth(&mut cmd, true).is_ok());
    std::env::remove_var("ANTHROPIC_API_KEY");
    assert!(apply_auth(&mut cmd, false).is_ok());
    assert!(matches!(
        apply_auth(&mut cmd, true),
        Err(EngineError::Posture(_))
    ));
}
