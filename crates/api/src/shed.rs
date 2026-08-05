//! Load shedding for the ingest routes — admission control for **load**, not spend.
//!
//! The limit rules in `limits`/`events` cap what a project may *spend*. Nothing capped what the
//! process may *attempt at once*: every ingest request dove straight into `spawn_blocking` and queued
//! behind the store's single connection mutex, with tokio's 512-thread blocking pool as the only
//! bound. Past saturation that is unbounded queueing — latency grows without limit, every client
//! waits on a request that will be stale by the time it lands, and an operator watching from outside
//! cannot tell a saturated server from a hung one.
//!
//! This module makes overload a **fast, explicit, countable rejection**:
//!
//! - a bounded number of ingest requests may be in flight ([`ENV_MAX_INFLIGHT`]); one past that is
//!   turned away immediately with **503 `overloaded`** and a `Retry-After`;
//! - an ingest request that outlives [`ENV_TIMEOUT_SECS`] is cut with **504 `timeout`**;
//! - both are counted, and the live in-flight depth is readable at `GET /v1/ingest/status`.
//!
//! **`overloaded` is deliberately not `rate_limited`.** They mean opposite things to a client: 429
//! `rate_limited` says *you* are over your budget and the event was refused on purpose (backing off
//! is the whole point, and retrying later may still fail until the window rolls); 503 `overloaded`
//! says the *server* is momentarily saturated and the identical request will succeed shortly. A
//! client that confused them would either hammer a struggling server or silently drop events it was
//! entitled to send.
//!
//! **Shedding never happens inside a store transaction.** The permit is taken before the handler
//! runs, so a shed request has touched nothing. A *timeout* can fire while a handler is awaiting its
//! blocking store call — but dropping that future does not cancel the `spawn_blocking` work, so a
//! batch transaction still commits or rolls back on its own terms; the client simply learns nothing
//! about it, exactly as if its own HTTP client had timed out. That case is already covered by the
//! replay-safe ingest contract: resending the same id is acknowledged, never double-counted.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use tokio::sync::Semaphore;

use crate::error::{ApiError, ErrorCode};
use crate::guards::authenticate;
use crate::state::AppState;

/// Env: max ingest requests allowed in flight at once. `0` disables shedding (unbounded, the old
/// behavior). Default [`DEFAULT_MAX_INFLIGHT`].
const ENV_MAX_INFLIGHT: &str = "LIGHTTRACK_INGEST_MAX_INFLIGHT";
/// Env: seconds an ingest request may run before it is cut with 504. `0` disables the timeout.
const ENV_TIMEOUT_SECS: &str = "LIGHTTRACK_INGEST_TIMEOUT_SECS";
/// Env: the `Retry-After` (seconds) advertised on a shed response.
const ENV_RETRY_AFTER_SECS: &str = "LIGHTTRACK_INGEST_RETRY_AFTER_SECS";

/// Writes serialize on one store lock, so the useful concurrency is small; the cap exists to bound
/// the *queue*, not to find a throughput sweet spot. 64 leaves ample room for bursts while keeping
/// worst-case queueing to tens of requests rather than the blocking pool's 512.
const DEFAULT_MAX_INFLIGHT: usize = 64;
const DEFAULT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_RETRY_AFTER_SECS: u64 = 1;

/// Bounded-concurrency gate for the ingest routes, plus the counters an operator reads.
pub(crate) struct IngestGuard {
    /// `None` when shedding is disabled.
    permits: Option<Arc<Semaphore>>,
    max_in_flight: usize,
    /// `None` when the timeout is disabled.
    timeout: Option<Duration>,
    retry_after: u64,
    shed_total: AtomicU64,
    timeout_total: AtomicU64,
    admitted_total: AtomicU64,
}

impl IngestGuard {
    pub(crate) fn from_env() -> Self {
        let max_in_flight = env_usize(ENV_MAX_INFLIGHT).unwrap_or(DEFAULT_MAX_INFLIGHT);
        let timeout_secs = env_u64(ENV_TIMEOUT_SECS).unwrap_or(DEFAULT_TIMEOUT_SECS);
        Self {
            permits: (max_in_flight > 0).then(|| Arc::new(Semaphore::new(max_in_flight))),
            max_in_flight,
            timeout: (timeout_secs > 0).then(|| Duration::from_secs(timeout_secs)),
            retry_after: env_u64(ENV_RETRY_AFTER_SECS).unwrap_or(DEFAULT_RETRY_AFTER_SECS),
            shed_total: AtomicU64::new(0),
            timeout_total: AtomicU64::new(0),
            admitted_total: AtomicU64::new(0),
        }
    }

    /// A guard with an explicit cap and timeout, for tests.
    #[cfg(test)]
    pub(crate) fn with_limits(max_in_flight: usize, timeout: Option<Duration>) -> Self {
        Self {
            permits: (max_in_flight > 0).then(|| Arc::new(Semaphore::new(max_in_flight))),
            max_in_flight,
            timeout,
            retry_after: DEFAULT_RETRY_AFTER_SECS,
            shed_total: AtomicU64::new(0),
            timeout_total: AtomicU64::new(0),
            admitted_total: AtomicU64::new(0),
        }
    }

    /// Hold one of the guard's permits, so a test can saturate the gate deterministically instead of
    /// racing real requests against each other.
    #[cfg(test)]
    pub(crate) fn take_permit(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.permits.clone()?.try_acquire_owned().ok()
    }

    /// How many ingest requests are executing right now.
    fn in_flight(&self) -> usize {
        match &self.permits {
            Some(s) => self.max_in_flight.saturating_sub(s.available_permits()),
            None => 0,
        }
    }

    pub(crate) fn describe(&self) -> String {
        let cap = if self.permits.is_some() {
            self.max_in_flight.to_string()
        } else {
            "unbounded".to_string()
        };
        let t = match self.timeout {
            Some(d) => format!("{}s", d.as_secs()),
            None => "off".to_string(),
        };
        format!("max_inflight={cap}, timeout={t}")
    }
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok().and_then(|s| s.trim().parse().ok())
}
fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|s| s.trim().parse().ok())
}

/// Ingest admission middleware: take a permit or shed, then run the handler under a deadline.
///
/// Layered onto the ingest **POST** method routers only — a read that is slow is a nuisance, but a
/// write path that queues without bound is what turns a busy server into an unresponsive one, and
/// bounding only the writers keeps the operator's own diagnostic reads answerable while shedding.
pub(crate) async fn ingest_admission(
    State(st): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let guard = st.ingest_guard.clone();
    // `try_acquire_owned` is the whole point: it never waits. A request that cannot get a permit is
    // rejected in microseconds instead of joining a queue whose depth nobody bounds.
    let _permit = match &guard.permits {
        Some(sem) => match sem.clone().try_acquire_owned() {
            Ok(p) => Some(p),
            Err(_) => {
                guard.shed_total.fetch_add(1, Ordering::Relaxed);
                return shed_response(&guard);
            }
        },
        None => None,
    };
    guard.admitted_total.fetch_add(1, Ordering::Relaxed);

    match guard.timeout {
        None => next.run(req).await,
        Some(d) => match tokio::time::timeout(d, next.run(req)).await {
            Ok(resp) => resp,
            Err(_) => {
                guard.timeout_total.fetch_add(1, Ordering::Relaxed);
                ApiError::new(
                    ErrorCode::Timeout,
                    format!(
                        "ingest exceeded the {}s server deadline; the write may or may not have \
                         landed — resend the same event id to find out safely",
                        d.as_secs()
                    ),
                )
                .into_response()
            }
        },
    }
}

/// 503 + `Retry-After`, with a code that can never be mistaken for a budget 429.
fn shed_response(guard: &IngestGuard) -> Response {
    let mut resp = ApiError::new(
        ErrorCode::Overloaded,
        format!(
            "server is shedding ingest load ({} requests already in flight); retry shortly — this \
             is server saturation, not a usage limit",
            guard.max_in_flight
        ),
    )
    .into_response();
    if let Ok(v) = HeaderValue::from_str(&guard.retry_after.to_string()) {
        resp.headers_mut().insert("retry-after", v);
    }
    resp
}

/// The operator's saturation view: is the server shedding, and how hard?
#[derive(Serialize)]
pub(crate) struct IngestStatus {
    /// `None` when shedding is disabled (unbounded concurrency).
    max_in_flight: Option<usize>,
    in_flight: usize,
    /// Requests turned away with 503 `overloaded` since start.
    shed_total: u64,
    /// Requests cut with 504 `timeout` since start.
    timeout_total: u64,
    /// Requests that got a permit since start (the denominator for a shed rate).
    admitted_total: u64,
    timeout_secs: Option<u64>,
    retry_after_secs: u64,
}

/// `GET /v1/ingest/status` — counters are process-local and reset on restart, the same honesty as the
/// rejection ledger behind `/v1/limits/status`. Deliberately not a metrics-scrape surface; it is the
/// smallest thing that answers "are we shedding?" without a Prometheus stack.
pub(crate) async fn get_ingest_status(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<IngestStatus>, ApiError> {
    authenticate(&st, &headers).await?;
    let g = &st.ingest_guard;
    Ok(Json(IngestStatus {
        max_in_flight: g.permits.as_ref().map(|_| g.max_in_flight),
        in_flight: g.in_flight(),
        shed_total: g.shed_total.load(Ordering::Relaxed),
        timeout_total: g.timeout_total.load(Ordering::Relaxed),
        admitted_total: g.admitted_total.load(Ordering::Relaxed),
        timeout_secs: g.timeout.map(|d| d.as_secs()),
        retry_after_secs: g.retry_after,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_cap_reports_unbounded_and_never_sheds() {
        let g = IngestGuard::with_limits(0, None);
        assert!(g.permits.is_none());
        assert_eq!(g.in_flight(), 0);
        assert!(g.describe().contains("unbounded"));
    }

    #[test]
    fn in_flight_tracks_outstanding_permits() {
        let g = IngestGuard::with_limits(2, None);
        let sem = g.permits.clone().unwrap();
        assert_eq!(g.in_flight(), 0);
        let a = sem.clone().try_acquire_owned().unwrap();
        assert_eq!(g.in_flight(), 1);
        let b = sem.clone().try_acquire_owned().unwrap();
        assert_eq!(g.in_flight(), 2);
        // Full: the next attempt fails immediately rather than queueing.
        assert!(sem.clone().try_acquire_owned().is_err());
        drop(a);
        assert_eq!(g.in_flight(), 1);
        drop(b);
        assert_eq!(g.in_flight(), 0);
    }

    #[test]
    fn shed_response_is_503_with_retry_after_and_a_non_429_code() {
        let g = IngestGuard::with_limits(1, None);
        let resp = shed_response(&g);
        assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers().get("retry-after").unwrap(), "1");
        // The whole point of the distinction — a shed is never a rate limit.
        assert_ne!(
            ErrorCode::Overloaded.status(),
            ErrorCode::RateLimited.status()
        );
        assert_ne!(
            ErrorCode::Overloaded.as_str(),
            ErrorCode::RateLimited.as_str()
        );
    }
}
