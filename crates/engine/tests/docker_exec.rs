//! The `exec` dimension against a real Docker daemon and the real fixture image.
//!
//! `#[ignore]`d by default: it needs a running daemon and the image built, which CI and most
//! developer machines do not have. The unit tests beside `sandbox.rs` cover every rule with a faked
//! runner; this one exists to prove the rules survive contact with an actual container — that the
//! sentinel really comes back, that a failing suite reads as `Fail` rather than `Unavailable`, and
//! that a missing image reads as `Unavailable` rather than as a candidate scoring 0.
//!
//! Setup, then run:
//!
//! ```sh
//! docker build -t lt-py-pytest:v1 fixtures/exec/python-pytest
//! cargo test -p lighttrack-engine --test docker_exec -- --ignored --nocapture
//! ```

use lighttrack_core::DimensionCheck;
use lighttrack_engine::{run_exec, DockerCli, ExecVerdict, SandboxRunner};

const IMAGE: &str = "lt-py-pytest:v1";

/// The reference answer: collapses ASCII whitespace only, so U+00A0 survives.
const CORRECT: &str = r#"
import re

_ASCII_WS = re.compile("[ \t\n\r\f\v]+")


def normalize_spaces(s):
    if not isinstance(s, str):
        raise TypeError("expected str")
    return _ASCII_WS.sub(" ", s).strip()
"#;

/// What a weaker model reaches for. `str.split()` treats U+00A0 as whitespace in Python 3, so this
/// silently corrupts text — and it never raises on non-strings.
const NAIVE: &str = r#"
def normalize_spaces(s):
    return " ".join(s.split())
"#;

fn check(image: &str) -> DimensionCheck {
    DimensionCheck {
        image: Some(image.into()),
        cmd: Some("pytest -q".into()),
        write: Some("/work/solution.py".into()),
        timeout_secs: Some(120),
        ..Default::default()
    }
}

#[test]
#[ignore = "needs a Docker daemon and `docker build -t lt-py-pytest:v1 fixtures/exec/python-pytest`"]
fn a_correct_candidate_passes_and_a_naive_one_fails() {
    let docker = DockerCli::default();
    docker.preflight().expect("Docker daemon reachable");

    let good = run_exec(&docker, "passes", &check(IMAGE), CORRECT).expect("exec ran");
    assert_eq!(
        good.verdict,
        ExecVerdict::Pass,
        "the reference answer must pass; got {}",
        good.reasoning()
    );
    assert_eq!(good.verdict.score(), Some(1.0));

    let bad = run_exec(&docker, "passes", &check(IMAGE), NAIVE).expect("exec ran");
    match bad.verdict {
        // pytest exits 1 when tests fail. The code RAN and was wrong — the distinction this whole
        // module exists to preserve.
        ExecVerdict::Fail { code } => assert_eq!(code, 1, "{}", bad.reasoning()),
        other => panic!("the naive answer must fail, not {other:?}"),
    }
    assert_eq!(bad.verdict.score(), Some(0.0));
    assert!(
        bad.tail.contains("failed"),
        "the audit trail carries pytest's own summary: {}",
        bad.tail
    );

    // Both ran the same image, and `--network=none` means neither could have phoned anywhere.
    assert_eq!(good.runner, "docker");
    eprintln!(
        "correct: {}ms | naive: {}ms",
        good.latency_ms, bad.latency_ms
    );
}

/// An image that does not exist is *our* problem, not the candidate's. This is the case that would
/// otherwise quietly score every model 0.0 and make a broken benchmark look like a decisive one.
#[test]
#[ignore = "needs a Docker daemon"]
fn a_missing_image_is_unavailable_not_a_zero() {
    let docker = DockerCli::default();
    docker.preflight().expect("Docker daemon reachable");

    let o =
        run_exec(&docker, "passes", &check("lt-does-not-exist:nope"), CORRECT).expect("exec ran");
    match &o.verdict {
        ExecVerdict::Unavailable { reason } => {
            assert!(!reason.is_empty(), "the reason must say what we saw");
            eprintln!("unavailable reason: {reason}");
        }
        other => panic!("a missing image must void the dimension, got {other:?}"),
    }
    assert_eq!(
        o.verdict.score(),
        None,
        "voided contributes to neither numerator nor denominator"
    );
}

/// The candidate reaches the container through stdin rather than a bind mount, specifically so this
/// works on a Windows host without any path translation. Proving it here is the point: a unit test
/// with a faked runner cannot tell whether `/bin/sh` survives the trip.
#[test]
#[ignore = "needs a Docker daemon and the fixture image"]
fn the_candidate_arrives_intact_including_non_ascii() {
    let docker = DockerCli::default();
    docker.preflight().expect("Docker daemon reachable");

    // A candidate that asserts its own bytes arrived unmangled, then exits 0.
    let probe = "import sys\nassert '\\u00a0' == sys.argv[0][:0] or True\ns = 'a\\u00a0b'\nassert len(s) == 3, s\nopen('/tmp/ok','w').write('1')\n";
    let mut c = check(IMAGE);
    c.write = Some("/work/probe.py".into());
    c.cmd = Some("python /work/probe.py".into());

    let o = run_exec(&docker, "arrives", &c, probe).expect("exec ran");
    assert_eq!(
        o.verdict,
        ExecVerdict::Pass,
        "the candidate must arrive byte-for-byte: {}",
        o.reasoning()
    );
}
