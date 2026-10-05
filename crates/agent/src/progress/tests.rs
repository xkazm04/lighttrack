use std::sync::atomic::AtomicU32;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::*;

/// A sink that records what it was asked to publish, and can be made to fail or to be slow — the
/// two ways a telemetry channel gets to hurt the work it reports on, if it is allowed to.
struct Recorder {
    lines: Mutex<Vec<String>>,
    fail: bool,
    delay: Duration,
}

impl Recorder {
    fn new() -> Self {
        Recorder {
            lines: Mutex::new(Vec::new()),
            fail: false,
            delay: Duration::ZERO,
        }
    }

    fn failing() -> Self {
        Recorder {
            fail: true,
            ..Recorder::new()
        }
    }

    fn slow(delay: Duration) -> Self {
        Recorder {
            delay,
            ..Recorder::new()
        }
    }

    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Sink for Recorder {
    fn publish(&self, line: &str) -> anyhow::Result<()> {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        self.lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(line.to_string());
        if self.fail {
            anyhow::bail!("POST /progress -> HTTP 409: not held");
        }
        Ok(())
    }
}

fn resolving() -> Phase {
    Phase::Resolving {
        action: "ns/reprice-summary".to_string(),
        version: Some("v3".to_string()),
    }
}

fn spawned() -> Phase {
    Phase::Spawned {
        mode: "edit",
        workspace: Some("sales-repo".to_string()),
        model: "claude-opus-5".to_string(),
        fingerprint: "a1b2c3d4e5f60718293a4b5c6d7e8f90".to_string(),
    }
}

fn shown<'a>(secs: u64, renewals: u32, fingerprint: Option<&'a str>) -> Shown<'a> {
    Shown {
        elapsed: Duration::from_secs(secs),
        renewals,
        fingerprint,
        connector: None,
    }
}

/// CASE 1 — the operator's question. A published line has to name the phase, how old the run is and
/// which renewal it is on; a bare "working" is the state the record was already in (NULL).
#[test]
fn a_published_line_names_the_phase_its_age_and_its_renewal() {
    let digest = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
    let line = render(&spawned(), &shown(14 * 60 + 3, 3, Some(digest)));
    assert!(line.contains("running"), "{line}");
    assert!(line.contains("edit"), "{line}");
    assert!(line.contains("sales-repo"), "{line}");
    assert!(line.contains("14m"), "{line}");
    assert!(line.contains("renewal 3"), "{line}");

    // The fingerprint is a PREFIX: enough to join a run to a prompt version, not the whole digest.
    assert!(line.contains("a1b2c3d4"), "{line}");
    assert!(
        !line.contains(digest),
        "the whole digest does not belong on a liveness line: {line}"
    );

    // And the phases have to be told apart, or the field answers nothing.
    let a = render(&resolving(), &shown(1, 0, None));
    let b = render(&Phase::Settling, &shown(1, 0, None));
    assert_ne!(a, b);
    assert!(a.contains("ns/reprice-summary") && a.contains("v3"), "{a}");

    // The connector kind rides the delivering phase when the action declares one.
    let delivering = render(
        &Phase::Delivering,
        &Shown {
            connector: Some("http"),
            ..shown(5, 1, None)
        },
    );
    assert!(delivering.contains("http"), "{delivering}");
}

/// CASE 2 + CASE 6 — a phase CHANGE is published within one throttle window, not at the next
/// renewal; and whatever was published last names the phase the run was in, which is exactly what
/// an operator reads from storage alone when the device died there.
#[test]
fn a_phase_change_is_published_within_one_window_and_the_last_line_names_the_phase() {
    let rec = Recorder::new();
    let (px, rx) = channel();
    let window = Duration::from_millis(40);
    let renewals = AtomicU32::new(0);

    std::thread::scope(|s| {
        s.spawn(|| pump(&rec, rx, window, &renewals, "cloud"));
        px.emit(resolving());
        std::thread::sleep(Duration::from_millis(15));
        let early = rec.lines();
        assert_eq!(
            early.len(),
            1,
            "the first phase publishes at once, not after a window: {early:?}"
        );
        assert!(early[0].contains("ns/reprice-summary"), "{early:?}");

        px.emit(spawned());
        std::thread::sleep(window * 3);
        let later = rec.lines();
        assert!(
            later.len() >= 2,
            "a phase change must publish within one window: {later:?}"
        );
        assert!(
            later.last().expect("a line").contains("sales-repo"),
            "{later:?}"
        );
        drop(px);
    });

    let last = rec.lines().last().cloned().expect("a published line");
    assert!(
        last.contains("running"),
        "the last line published is where the run was: {last}"
    );
}

/// CASE 4 — a chatty phase must not flood the cloud: one publish per window, however many
/// emissions arrive inside it.
#[test]
fn a_hundred_emissions_inside_one_window_cost_one_publish() {
    let mut throttle = Throttle::new(Duration::from_secs(10));
    let at = Duration::from_secs(1);
    let published = (0..100).filter(|_| throttle.due(at)).count();
    assert_eq!(
        published, 1,
        "100 emissions inside one throttle window must cost exactly one HTTP publish"
    );
    assert!(
        throttle.due(Duration::from_secs(11)),
        "the next window publishes again"
    );
    assert!(
        !throttle.due(Duration::from_secs(11)),
        "and only once in that one too"
    );
}

/// CASE 3a — a refused publish (the 409 the fenced UPDATE answers when the lease moved on) is
/// swallowed: the pump keeps going, and the emitter's signature gives the caller nothing to
/// propagate. Progress is a read model; only the renewal decides ownership.
#[test]
fn a_refused_publish_is_swallowed_and_the_pump_keeps_going() {
    let rec = Recorder::failing();
    let (px, rx) = channel();
    let window = Duration::from_millis(20);
    let renewals = AtomicU32::new(0);

    std::thread::scope(|s| {
        s.spawn(|| pump(&rec, rx, window, &renewals, "cloud"));
        px.emit(resolving());
        std::thread::sleep(Duration::from_millis(10));
        px.emit(spawned());
        std::thread::sleep(window * 4);
        drop(px);
    });

    let lines = rec.lines();
    assert!(
        lines.len() >= 2,
        "a 409 on one publish must not end the channel: {lines:?}"
    );
    // `emit` returns unit: there is no error for the run to notice, by construction.
    let unit: () = Emitter::detached().emit(Phase::Settling);
    assert_eq!(unit, ());
}

/// CASE 3b — THE property. A slow or hung publish must never sit on the thread doing the work, and
/// therefore never on the path that would otherwise renew the lease. Emission is a non-blocking
/// handoff; the HTTP call happens on the pump's own thread.
#[test]
fn emitting_cannot_block_the_thread_that_does_the_work() {
    let rec = Recorder::slow(Duration::from_millis(400));
    let (px, rx) = channel();
    let renewals = AtomicU32::new(0);

    std::thread::scope(|s| {
        s.spawn(|| pump(&rec, rx, Duration::from_secs(10), &renewals, "cloud"));
        let t0 = Instant::now();
        for _ in 0..50 {
            px.emit(spawned());
        }
        let spent = t0.elapsed();
        assert!(
            spent < Duration::from_millis(100),
            "50 emissions against a 400ms-per-publish sink took {spent:?}; a progress POST must \
             never be on the work's (or the renewal's) thread"
        );
        drop(px);
    });
}

/// CASE 5 — GUARD: the device-side privacy default. The phase vocabulary has nowhere to put prompt
/// or result text, so an action with `report_io = false` publishes nothing new about its content.
#[test]
fn guard_no_phase_can_carry_prompt_or_result_text() {
    const PROMPT: &str = "Price part A-1 for Acme at the 2026 contract rate";
    const RESULT: &str = "the margin is 31.4%";
    for phase in [
        resolving(),
        spawned(),
        Phase::Exited {
            subtype: "success".to_string(),
            connector: Some("http"),
        },
        Phase::Delivering,
        Phase::Settling,
    ] {
        let line = render(
            &phase,
            &Shown {
                connector: Some("http"),
                ..shown(90, 2, Some("a1b2c3d4e5f6"))
            },
        );
        assert!(!line.contains(PROMPT), "{line}");
        assert!(!line.contains("Acme"), "{line}");
        assert!(!line.contains(RESULT), "{line}");
        assert!(!line.contains("31.4"), "{line}");
    }
}

/// CASE 7 — GUARD: progress never rides the heartbeat. The renewal body stays `fence` only, because
/// liveness must not wait on the work having something to say — a device alive but stuck computing
/// a progress line would read as a dead one, and those are the two states the mechanism exists to
/// tell apart.
#[test]
fn guard_the_renewal_body_still_carries_fence_only() {
    let src = include_str!("../cloud.rs");
    let renew = src
        .split_once("pub(crate) fn renew(")
        .expect("cloud.rs declares renew")
        .1;
    let body = renew
        .split_once("pub(crate) fn ")
        .map(|(b, _)| b)
        .unwrap_or(renew);
    // Comments explain the design; only code may be matched against (a matcher that reads prose
    // reports whatever the prose happens to mention).
    let code: String = body
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(code.contains("fence"), "the renewal carries the fence");
    assert!(
        !code.contains("progress"),
        "no progress field may be added to /renew: {code}"
    );
    assert!(
        code.contains("/renew"),
        "and it is still its own door: {code}"
    );
}
