//! The provider boundary, exercised end-to-end against a stub that speaks real HTTP.
//!
//! What this suite is for: the retry ladder's two new decisions — honour the provider's *stated*
//! delay, and end the ladder rather than truncate a wait that does not fit the budget — are only
//! true if the header survives the whole path (wire → `reqwest` → status/header classification →
//! typed `EngineError` → scheduler). A test that fakes the closure passed to `with_retry` asserts
//! the scheduler and skips the two joints where this actually broke before, which is why the ladder
//! never read the header tracklight's own API emits.
//!
//! Why a hand-rolled stub and not `wiremock`/`httpmock`/`mockito`: the engine's client is
//! **blocking** `reqwest`, and every one of those crates is async and pulls a tokio runtime into a
//! crate that has none — a large dependency (and a CI/audit surface, see `deny.toml`) bought to
//! serve a listener that answers one canned response. `std::net::TcpListener` on loopback is ~40
//! lines, adds nothing to `Cargo.toml`, and is a *more* faithful boundary: it counts connections at
//! the wire, which is the instrument the measurable needs.
//!
//! The call path is the real one. `LIGHTTRACK_OPENAI_BASE` re-points the origin; nothing else about
//! `generate` is mocked.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use lighttrack_engine::{generate, EngineConfig, EngineError};

/// `generate` reads its origin and key from the process environment, which every test in this
/// binary shares. Held for the length of a test so two of them cannot re-point each other's stub.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// A loopback endpoint that answers every request with one canned response and records when each
/// request arrived. `Connection: close` makes the count exact: one connection per attempt, no
/// keep-alive reuse to reason about.
struct Stub {
    base: String,
    hits: Arc<Mutex<Vec<Instant>>>,
}

impl Stub {
    fn attempts(&self) -> usize {
        self.hits.lock().unwrap().len()
    }

    /// Gaps between consecutive attempts — the ladder's spacing, measured at the wire.
    fn gaps(&self) -> Vec<Duration> {
        let hits = self.hits.lock().unwrap();
        hits.windows(2).map(|w| w[1] - w[0]).collect()
    }
}

fn stub(status: u16, headers: &'static [(&'static str, &'static str)]) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hits);
    // Daemon: the test process exiting takes it with it.
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut sock) = conn else { continue };
            recorder.lock().unwrap().push(Instant::now());
            // The request body must be drained before answering, or the client sees a reset
            // mid-write instead of the status we are testing.
            let mut reader = BufReader::new(sock.try_clone().expect("clone socket"));
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let _ = reader.take(len as u64).read_to_end(&mut Vec::new());

            let body = r#"{"error":{"message":"stub"}}"#;
            let mut resp = format!(
                "HTTP/1.1 {status} STUB\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for (k, v) in headers {
                resp.push_str(&format!("{k}: {v}\r\n"));
            }
            resp.push_str("\r\n");
            resp.push_str(body);
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        }
    });
    Stub { base, hits }
}

/// Drive the real `generate` path at the stub.
fn call_openai(stub: &Stub) -> EngineError {
    std::env::set_var("OPENAI_API_KEY", "test-key-not-a-secret");
    std::env::set_var("LIGHTTRACK_OPENAI_BASE", &stub.base);
    let cfg = EngineConfig::default();
    generate(&cfg, "openai", "stub-model", None, "hello", None)
        .expect_err("the stub never returns a completion")
}

/// **The baseline number.** Without a stated delay the ladder is the computed one: three attempts
/// spanning ~600ms, ending in the rate-limit error. This is what the second measurable is measured
/// against, and it must not regress.
#[test]
fn a_bare_429_spends_the_computed_ladder() {
    let _guard = env_lock();
    let s = stub(429, &[]);
    let err = call_openai(&s);
    assert!(
        matches!(
            err,
            EngineError::RateLimited {
                retry_after: None,
                ..
            }
        ),
        "got {err:?}"
    );
    assert_eq!(s.attempts(), 3, "1 initial + 2 retries");
    let gaps = s.gaps();
    assert!(
        gaps[0] >= Duration::from_millis(200) && gaps[1] >= Duration::from_millis(400),
        "computed ladder must still double: {gaps:?}"
    );
}

/// **Attempts per 429, with the header the provider actually sent.** The stated delay reaches the
/// scheduler across the whole boundary and replaces the computed rung: the attempts are spaced by
/// what the provider asked for (`retry-after-ms: 120`), not by the ladder's own 200/400.
#[test]
fn a_stated_delay_reaches_the_scheduler_and_spaces_the_attempts() {
    let _guard = env_lock();
    let s = stub(429, &[("retry-after-ms", "120")]);
    let err = call_openai(&s);
    match err {
        EngineError::RateLimited { retry_after, .. } => assert_eq!(
            retry_after,
            Some(Duration::from_millis(120)),
            "the stated delay must survive to the typed error"
        ),
        other => panic!("expected RateLimited, got {other:?}"),
    }
    assert_eq!(s.attempts(), 3);
    for g in s.gaps() {
        assert!(
            g >= Duration::from_millis(120),
            "never retry earlier than the provider asked: {g:?}"
        );
    }
}

/// **The over-budget rule, and the terminal state it must be recorded as.** `Retry-After: 900`
/// (seconds) cannot fit a 60s call budget. The ladder ends: no truncated sleep, no second attempt,
/// and the error is neither `RateLimited` (which would read as exhaustion and lose the number) nor
/// a shortened wait.
#[test]
fn a_stated_wait_beyond_the_budget_ends_the_ladder() {
    let _guard = env_lock();
    let s = stub(429, &[("retry-after", "900")]);
    let started = Instant::now();
    let err = call_openai(&s);
    match err {
        EngineError::OverBudgetWait {
            who,
            asked_secs,
            remaining_secs,
            attempts,
        } => {
            assert_eq!(who, "openai");
            assert_eq!(asked_secs, 900.0, "the wait that did not fit is the record");
            assert!(remaining_secs > 0.0 && remaining_secs <= 60.0);
            assert_eq!(attempts, 1);
        }
        other => panic!("expected OverBudgetWait, got {other:?}"),
    }
    assert_eq!(
        s.attempts(),
        1,
        "must spend no attempt the provider already said would fail"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "it must fail fast rather than sleep out the budget"
    );
}
