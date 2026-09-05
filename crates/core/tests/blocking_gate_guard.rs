//! The gate the manifest calls blocking must be a gate the workflow actually blocks on.
//!
//! `.ai/manifest.yaml`'s `controls.ciHardPass` is the machine-readable claim about which capability
//! failures stop a merge, and `controls.ciAdvisory` is the claim about which ones deliberately do
//! not. Two guards already exist beside this one, and between them they left a hole exactly the
//! shape of the incident that prompted this file:
//!
//! * `manifest_guard.rs` binds `ciHardPass` to `scripts/gates.sh` — the LOCAL rung.
//! * `gate_table_guard.rs` binds `.github/workflows/ci.yml` to CONTRIBUTING.md's published table —
//!   two surfaces that can be edited together.
//!
//! Nothing bound the manifest's grade to the workflow's `continue-on-error:`. So a change that made
//! `cargo fmt --check` advisory and updated the doc's Blocking column in the same commit passed both
//! guards while the manifest — the file an agent reads to decide whether a red gate matters — went
//! on saying `format-check` was a hard pass. That is the failure this repository has already seen
//! once from the other side: a commit in its history reads "the formatting gate has been red on
//! main", which is only possible when a gate everyone believed was blocking was not.
//!
//! What this test asserts, in both directions:
//!
//! * every capability in `ciHardPass` names a CI job that exists AND has no `continue-on-error:
//!   true` — the grade is a property of the workflow, not of a document;
//! * every capability in `ciAdvisory` names a job that IS `continue-on-error: true` — an advisory
//!   lane that quietly starts walling PRs is the same drift in the other direction;
//! * for the gates whose command runs verbatim in CI (fmt and clippy among them), the mapped job
//!   really runs the manifest's command string — so a job can keep a blocking name while running
//!   something narrower.
//!
//! The capability→job map below is the one hand-written part, and it is held up by the rest: a name
//! that does not exist in `ci.yml` fails here, and `gate_table_guard.rs` independently requires the
//! same names in CONTRIBUTING.md — which is what branch protection is configured from.
//!
//! WHAT IT STILL CANNOT SEE: whether those check names are *marked required* in GitHub's branch
//! protection settings. That state lives in the repository's settings, not in the tree, and no test
//! in the tree can read it. `cargo fmt --check` blocking here means "the job fails the run"; making
//! a failed run block the merge is the one manual step, and CONTRIBUTING.md names the exact strings
//! to paste.

use std::collections::BTreeMap;

const CI_YML: &str = include_str!("../../../.github/workflows/ci.yml");
const MANIFEST: &str = include_str!("../../../.ai/manifest.yaml");

/// capability name in the manifest → the `name:` of the ci.yml job that runs it.
///
/// `judge-eval` maps to the workspace suite on purpose: it is a `cargo test` selector inside it, not
/// a job of its own.
const JOB_FOR: &[(&str, &str)] = &[
    ("test", "cargo test --workspace"),
    ("judge-eval", "cargo test --workspace"),
    ("conformance", "sqlite conformance (required)"),
    ("chart-policy", "chart policy (required)"),
    ("lint", "cargo clippy -D warnings"),
    ("format-check", "cargo fmt --check"),
    ("audit-policy", "cargo deny (policy)"),
    ("audit-secrets", "gitleaks (secrets)"),
    ("test-client-rust", "cargo test (rust sdk)"),
    ("test-client-python", "python suite (python sdk)"),
    ("test-client-typescript", "npm test (typescript sdk)"),
    ("audit-advisories", "cargo deny (advisories, advisory)"),
    ("audit-secrets-latest-rules", "gitleaks (latest rules, advisory)"),
];

/// The capabilities whose manifest command must appear VERBATIM in their job. The rest run through
/// a purpose-built action (`cargo-deny-action`) or as several steps with a `working-directory`, so
/// there is no single string to compare — those are covered by the grade assertions only.
const RUNS_VERBATIM: &[&str] = &[
    "test",
    "conformance",
    "chart-policy",
    "lint",
    "format-check",
    "audit-secrets",
    "test-client-rust",
];

/// One CI job: is it blocking, and what is its body?
struct Job {
    blocking: bool,
    body: String,
}

/// Parse `jobs:` — a job id is indented two spaces, `name:` and `continue-on-error:` four. Keyed by
/// the job's `name:`, because that is the string branch protection and CONTRIBUTING.md both spell.
fn jobs(yaml: &str) -> BTreeMap<String, Job> {
    let mut out: BTreeMap<String, Job> = BTreeMap::new();
    let lines: Vec<&str> = yaml.lines().collect();
    let Some(head) = lines.iter().position(|l| l.starts_with("jobs:")) else {
        return out;
    };
    // The jobs block runs until the next top-level key (there is none today, but a parser that
    // assumes that is a parser that silently swallows one).
    let mut end = lines.len();
    for (i, l) in lines.iter().enumerate().skip(head + 1) {
        if !l.starts_with(' ') && !l.trim().is_empty() {
            end = i;
            break;
        }
    }
    // Where each job starts: a key indented exactly two spaces.
    let mut starts: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate().take(end).skip(head + 1) {
        let t = line.trim_start();
        let indent = line.len() - t.len();
        if indent == 2 && t.ends_with(':') && !t.starts_with('#') {
            starts.push(i);
        }
    }
    for (n, &s) in starts.iter().enumerate() {
        let stop = starts.get(n + 1).copied().unwrap_or(end);
        let mut name = lines[s].trim().trim_end_matches(':').to_string();
        let mut blocking = true;
        let mut body = String::new();
        for line in &lines[s + 1..stop] {
            let t = line.trim_start();
            let indent = line.len() - t.len();
            if t.starts_with('#') || t.is_empty() {
                continue;
            }
            if indent == 4 {
                if let Some(v) = t.strip_prefix("name:") {
                    name = v.trim().trim_matches('"').trim_matches('\'').to_string();
                } else if let Some(v) = t.strip_prefix("continue-on-error:") {
                    blocking = v.trim() != "true";
                }
            }
            body.push_str(t);
            body.push('\n');
        }
        out.insert(name, Job { blocking, body });
    }
    out
}

/// `capability -> command`, from the manifest's one-line `capabilities:` entries.
fn commands(yaml: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut inside = false;
    for line in yaml.lines() {
        if line.starts_with("capabilities:") {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if !line.starts_with(' ') && !line.trim().is_empty() {
            break;
        }
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        let Some((key, rest)) = trimmed.split_once(':') else {
            continue;
        };
        let Some(quoted) = rest.split("command: \"").nth(1) else {
            continue;
        };
        let Some(cmd) = quoted.split('"').next() else {
            continue;
        };
        // The advisory-only trailing comment in one capability is not part of what runs.
        let cmd = cmd.split("  #").next().unwrap_or(cmd).trim();
        out.insert(key.trim().to_string(), cmd.to_string());
    }
    out
}

/// A flow sequence, folded across lines the way a formatter writes `controls.ciHardPass`.
fn control_lane(yaml: &str, lane: &str) -> Vec<String> {
    let mut inside = false;
    let mut collecting = false;
    let mut buf = String::new();
    for line in yaml.lines() {
        if line.starts_with("controls:") {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if !line.starts_with(' ') && !line.trim().is_empty() {
            break;
        }
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 2 {
            if collecting {
                break;
            }
            if let Some(v) = trimmed.strip_prefix(&format!("{lane}:")) {
                collecting = true;
                buf.push_str(v.trim());
            }
            continue;
        }
        if collecting {
            buf.push(' ');
            buf.push_str(trimmed);
        }
    }
    buf.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn job_for(capability: &str) -> Option<&'static str> {
    JOB_FOR
        .iter()
        .find(|(c, _)| *c == capability)
        .map(|(_, j)| *j)
}

#[test]
fn every_blocking_capability_names_a_ci_job_that_actually_blocks() {
    let ci = jobs(CI_YML);
    let cmds = commands(MANIFEST);
    let hard = control_lane(MANIFEST, "ciHardPass");
    assert!(ci.len() >= 5, "parsed only {} jobs from ci.yml", ci.len());
    assert!(hard.len() >= 5, "parsed only {} hard gates", hard.len());
    assert!(
        hard.iter().any(|c| c == "format-check") && hard.iter().any(|c| c == "lint"),
        "format-check and lint must stay declared blocking: {hard:?}"
    );

    for cap in &hard {
        let job_name = job_for(cap).unwrap_or_else(|| {
            panic!(
                "controls.ciHardPass declares '{cap}' blocking, but this guard has no ci.yml job \
                 mapped to it. Add the row to JOB_FOR in the same change that added the gate — an \
                 unmapped gate is a gate nobody proved runs remotely."
            )
        });
        let job = ci.get(job_name).unwrap_or_else(|| {
            panic!("'{cap}' maps to CI job '{job_name}', which ci.yml does not define")
        });
        assert!(
            job.blocking,
            "the manifest declares '{cap}' a hard pass, but ci.yml's '{job_name}' is \
             `continue-on-error: true` — a gate that cannot fail a run cannot block a merge. \
             Either drop the `continue-on-error:` or move '{cap}' to controls.ciAdvisory."
        );
        if RUNS_VERBATIM.contains(&cap.as_str()) {
            let cmd = &cmds[cap];
            assert!(
                job.body.contains(cmd),
                "ci.yml's '{job_name}' does not run the '{cap}' command the manifest declares:\n  \
                 {cmd}\nA job can keep a blocking name while running something narrower; this is \
                 what stops that."
            );
        }
    }
}

#[test]
fn every_advisory_capability_names_a_ci_job_that_actually_does_not_block() {
    // The same drift in the other direction. Both advisory lanes read an input that moves WITHOUT
    // this repository (the RUSTSEC feed, gitleaks' upstream rules), so neither can be made green by
    // work here — promoting one would wall every unrelated PR on somebody else's publication.
    let ci = jobs(CI_YML);
    let advisory = control_lane(MANIFEST, "ciAdvisory");
    assert!(!advisory.is_empty(), "controls.ciAdvisory did not parse");
    for cap in &advisory {
        let job_name =
            job_for(cap).unwrap_or_else(|| panic!("no ci.yml job is mapped to advisory '{cap}'"));
        let job = ci.get(job_name).unwrap_or_else(|| {
            panic!("'{cap}' maps to CI job '{job_name}', which ci.yml does not define")
        });
        assert!(
            !job.blocking,
            "the manifest declares '{cap}' advisory, but ci.yml's '{job_name}' blocks. One of the \
             two is wrong, and the manifest is what an agent reads to decide whether a red gate \
             matters."
        );
    }
}

#[test]
fn the_guard_can_go_red() {
    // Seeded failure: each parser must find something, and must disagree when the surfaces drift.
    let yaml = "jobs:\n  a:\n    name: cargo fmt --check\n    steps:\n      - run: cargo fmt --all -- --check\n  b:\n    name: advisory thing\n    continue-on-error: true\n";
    let parsed = jobs(yaml);
    assert_eq!(parsed.len(), 2);
    let fmt = &parsed["cargo fmt --check"];
    assert!(fmt.blocking);
    assert!(fmt.body.contains("cargo fmt --all -- --check"));
    assert!(
        !parsed["advisory thing"].blocking,
        "continue-on-error ⇒ advisory, and that is the drift the real test catches"
    );

    let manifest = "capabilities:\n  format-check: { command: \"cargo fmt --all -- --check\", verified: false }\ncontrols:\n  ciHardPass:\n    [\n      format-check,\n    ]\n  ciAdvisory: [x]\n";
    let parsed_cmds = commands(manifest);
    assert_eq!(parsed_cmds["format-check"], "cargo fmt --all -- --check");
    assert_eq!(control_lane(manifest, "ciHardPass"), ["format-check"]);
    assert_eq!(control_lane(manifest, "ciAdvisory"), ["x"]);

    // And the map is complete in both directions for the real files.
    let hard = control_lane(MANIFEST, "ciHardPass");
    let adv = control_lane(MANIFEST, "ciAdvisory");
    for (cap, _) in JOB_FOR {
        assert!(
            hard.iter().any(|c| c == cap) || adv.iter().any(|c| c == cap),
            "JOB_FOR maps '{cap}', which no control lane names — a stale row proves nothing"
        );
    }
}
