//! Make a failed send *visible* without ever making it *panic*.
//!
//! The client is fire-and-forget by contract: telemetry must never break the host app, so the worker
//! discarded every send result with `let _ = req.send()`. Swallowing everything, though, also
//! swallowed the failure every first-time user hits — follow the README with no project configured,
//! the API answers `400 project_id is required`, the event vanishes, and nothing at all is printed.
//!
//! So: still never panic, never block the caller, never touch stdout (the host app may be speaking a
//! protocol on it) — but write one actionable line to **stderr**, rate-limited per error kind so a
//! tight loop of failing calls warns once rather than thousands of times.
//!
//! Silence it with `LIGHTTRACK_QUIET=1` or [`Client::quiet`](crate::Client::quiet).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) const PREFIX: &str = "[lighttrack]";
/// One line per error kind per this interval. A persistent outage still re-warns (reporting what was
/// suppressed) instead of going quiet forever after the first line.
pub(crate) const COOLDOWN: Duration = Duration::from_secs(60);
const SILENCE_HINT: &str = "silence these warnings with LIGHTTRACK_QUIET=1 or Client::quiet(true)";

fn env_quiet() -> bool {
    std::env::var("LIGHTTRACK_QUIET")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

pub(crate) fn truncate(s: &str, limit: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(limit) {
        None => flat,
        Some((cut, _)) => format!("{}...", &flat[..cut]),
    }
}

#[derive(Default)]
struct Seen {
    /// kind -> (last emitted, how many were suppressed since).
    kinds: HashMap<String, (Instant, u64)>,
    emitted: u64,
    suppressed: u64,
}

/// Rate-limited stderr warner, shared between the caller and the background worker via `Arc`.
pub(crate) struct Diagnostics {
    quiet: AtomicBool,
    cooldown: Duration,
    seen: Mutex<Seen>,
}

impl Diagnostics {
    pub(crate) fn from_env() -> Self {
        Self { quiet: AtomicBool::new(env_quiet()), cooldown: COOLDOWN, seen: Mutex::new(Seen::default()) }
    }

    #[cfg(test)]
    fn with_cooldown(cooldown: Duration) -> Self {
        Self { quiet: AtomicBool::new(false), cooldown, seen: Mutex::new(Seen::default()) }
    }

    /// Toggle silence after construction — the worker thread already holds a clone of the `Arc`, so
    /// this has to be interior mutability rather than a plain field.
    pub(crate) fn set_quiet(&self, quiet: bool) {
        self.quiet.store(quiet, Ordering::Relaxed);
    }

    /// Emit `message` at most once per `kind` per cooldown window. Never panics: a poisoned lock or a
    /// closed stderr must not become the failure it is reporting.
    pub(crate) fn warn(&self, kind: &str, message: &str) {
        if self.quiet.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        let (held, first_line) = {
            let Ok(mut seen) = self.seen.lock() else { return };
            if let Some((last, held)) = seen.kinds.get_mut(kind) {
                if now.duration_since(*last) < self.cooldown {
                    *held += 1;
                    seen.suppressed += 1;
                    return;
                }
            }
            let held = seen.kinds.insert(kind.to_string(), (now, 0)).map(|(_, h)| h).unwrap_or(0);
            seen.emitted += 1;
            (held, seen.emitted == 1)
        };
        let repeat = if held > 0 {
            format!(" [{held} more suppressed in the last {}s]", self.cooldown.as_secs())
        } else {
            String::new()
        };
        let hint = if first_line { format!("\n  {PREFIX} {SILENCE_HINT}") } else { String::new() };
        eprintln!("{PREFIX} {message}{repeat}{hint}");
    }

    #[cfg(test)]
    fn counts(&self) -> (u64, u64) {
        let seen = self.seen.lock().unwrap();
        (seen.emitted, seen.suppressed)
    }
}

/// No project *and* no API key: the server has nothing to attribute these events to, so where they
/// land depends on how it is configured. Reported before the network call, so the user learns it on
/// the very first call rather than after a round trip.
///
/// Deliberately not phrased as a failure. A dev-mode server files unattributed events under a
/// `default` project, so this is a "you may not be getting what you expect" notice, not an error;
/// only an authenticating server actually turns them away.
///
/// Messages stay ASCII-only: they land in whatever console the host app has, and a cp1252 Windows
/// terminal turns a stray em dash into mojibake.
pub(crate) fn no_project_message(base_url: &str) -> String {
    format!(
        "no project is configured, so these events are not attributed: a dev-mode server files them \
         under the 'default' project, and a server with authentication enabled rejects them. To \
         choose where they land, set LIGHTTRACK_PROJECT=<your-project-id> (or pass it to \
         Client::new), or set LIGHTTRACK_KEY to a project API key, which pins the project \
         server-side. Target: {base_url}"
    )
}

/// Context that decides which hint a failure gets.
#[derive(Clone, Copy, Default)]
pub(crate) struct FailureContext {
    pub status: Option<u16>,
    pub has_project: bool,
    pub has_key: bool,
}

pub(crate) fn send_failure_message(
    base_url: &str,
    path: &str,
    detail: &str,
    ctx: FailureContext,
) -> String {
    let hint = failure_hint(base_url, ctx);
    let sep = if hint.is_empty() { "" } else { " " };
    format!("event not sent to {base_url}{path}: {detail}.{sep}{hint}")
}

fn failure_hint(base_url: &str, ctx: FailureContext) -> String {
    let Some(status) = ctx.status else {
        return format!(
            "Is a LightTrack server running and reachable at {base_url}? Check LIGHTTRACK_URL. \
             Events are dropped while it is unreachable."
        );
    };
    match status {
        // The same trap as `no_project_message`, reached the slow way: a key was set (an *admin*
        // key, which pins no project) so the preflight check passed and the server did the rejecting.
        400 if !ctx.has_project => "The server has no project for this event. Fix: set \
             LIGHTTRACK_PROJECT=<your-project-id> (or pass it to Client::new), or use a *project* \
             API key in LIGHTTRACK_KEY; an admin key does not imply a project."
            .to_string(),
        400 => "The event was rejected as invalid: check provider / model / usage.".to_string(),
        401 | 403 if ctx.has_key => {
            "The key was rejected. Set LIGHTTRACK_KEY to a valid project or admin key.".to_string()
        }
        401 | 403 => {
            "This server requires authentication. Set LIGHTTRACK_KEY to a project API key.".to_string()
        }
        404 => format!("No such endpoint - is LIGHTTRACK_URL ({base_url}) pointing at a LightTrack API?"),
        429 => "The project is over a configured usage limit, so ingest is being refused.".to_string(),
        s if s >= 500 => "The LightTrack server errored; events are dropped until it recovers.".to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tight_loop_of_one_kind_warns_once() {
        let d = Diagnostics::with_cooldown(COOLDOWN);
        for _ in 0..1000 {
            d.warn("network", "boom");
        }
        assert_eq!(d.counts(), (1, 999), "a tight loop must not flood the console");
    }

    #[test]
    fn distinct_kinds_each_get_a_line() {
        let d = Diagnostics::with_cooldown(COOLDOWN);
        d.warn("network", "a");
        d.warn("http-400", "b");
        d.warn("network", "a");
        assert_eq!(d.counts().0, 2);
    }

    #[test]
    fn cooldown_expiry_re_warns() {
        let d = Diagnostics::with_cooldown(Duration::ZERO);
        d.warn("network", "boom");
        d.warn("network", "boom");
        assert_eq!(d.counts().0, 2);
    }

    #[test]
    fn quiet_silences_everything() {
        let d = Diagnostics::with_cooldown(COOLDOWN);
        d.set_quiet(true);
        d.warn("network", "boom");
        assert_eq!(d.counts(), (0, 0));
    }

    #[test]
    fn the_first_run_message_names_the_env_var() {
        let m = no_project_message("http://127.0.0.1:8787");
        assert!(m.contains("LIGHTTRACK_PROJECT"), "{m}");
        assert!(m.contains("LIGHTTRACK_KEY"), "{m}");
    }

    #[test]
    fn http_400_without_a_project_points_at_the_project_setting() {
        let m = send_failure_message(
            "http://h",
            "/v1/events",
            "HTTP 400 project_id is required",
            FailureContext { status: Some(400), has_project: false, has_key: true },
        );
        assert!(m.contains("LIGHTTRACK_PROJECT"), "{m}");
        assert!(m.contains("project_id is required"), "{m}");
    }

    #[test]
    fn an_unreachable_server_points_at_the_url_setting() {
        let m = send_failure_message("http://127.0.0.1:1", "/v1/events", "connection refused",
                                     FailureContext::default());
        assert!(m.contains("LIGHTTRACK_URL"), "{m}");
    }

    #[test]
    fn messages_are_ascii_only() {
        // They land in whatever console the host app has; a cp1252 terminal mangles anything else.
        for m in [
            no_project_message("http://h"),
            send_failure_message("http://h", "/v1/events", "x",
                                 FailureContext { status: Some(429), ..Default::default() }),
            send_failure_message("http://h", "/v1/events", "x",
                                 FailureContext { status: Some(503), ..Default::default() }),
        ] {
            assert!(m.is_ascii(), "non-ASCII in: {m}");
        }
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("a  b\nc", 100), "a b c");
        assert_eq!(truncate(&"x".repeat(300), 10), "xxxxxxxxxx...");
    }
}
