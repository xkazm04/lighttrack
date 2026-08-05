//! Per-source throttling of **failed** authentication attempts.
//!
//! `shed` bounds how many ingest requests may be *in flight*. Nothing bounded how many **credential
//! guesses** a caller could make: every rejected bearer token cost the attacker one round trip and
//! nothing else. Project keys carry ~244 bits of secret, so guessing one is not the threat — the
//! admin key (`LIGHTTRACK_ADMIN_KEY`) is **operator-chosen**, and a weak one was guessable as fast as
//! the network allowed. This module bounds that rate.
//!
//! ## Shape
//! - Only **failures** are counted. A request that authenticates is never counted and *clears* the
//!   source's record, so a legitimate client can never throttle itself with its own traffic.
//! - Past the configured budget, the source's next attempt is refused with **429 `rate_limited`** +
//!   `Retry-After`, **before the credential is compared**. That ordering is the whole point: a check
//!   that ran afterwards would relabel the response without slowing the guessing down at all.
//! - The refusal reuses the existing `rate_limited` code rather than inventing one. The wire contract
//!   in `error` is deliberately frozen, and `rate_limited`/429/`Retry-After` already means exactly
//!   "you are making too many of these, back off"; the message says which budget was spent. A
//!   *credential* throttle and a *spend* limit ask a client for the same behaviour — unlike 503
//!   `overloaded`, which had to stay distinct because it asks for the opposite one.
//!
//! ## Deliberate trade-off: a success clears the record
//! A source that authenticates gets a clean slate. That is what keeps one misconfigured client from
//! taking down its healthy neighbours behind the same egress address — for an observability tool,
//! losing telemetry *during* a misconfiguration is the worse failure. The cost is that an attacker
//! who already holds a valid credential (or shares a source with someone who does) can reset their
//! own budget. That is a much smaller threat than the anonymous internet brute-force this exists to
//! stop, and once a source is over the threshold the clearing stops: the throttle refuses before it
//! validates, so nothing can authenticate its way out of an active block.
//!
//! ## Where things live
//! - [`budget`] — the per-source failure counter: thresholds, the window, and the memory bound.
//! - [`source`] — *who* is calling, which is the part an attacker gets to lie about.

mod budget;
mod source;

pub(crate) use budget::AuthThrottle;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::error::ApiError;
use crate::state::AppState;

tokio::task_local! {
    /// Throttle key for the request being served, established by [`source_scope`].
    ///
    /// A task-local rather than a parameter because `guards::authenticate` takes only `&HeaderMap`
    /// across ~50 call sites while the peer address lives in the request's extensions. The
    /// alternative — stamping the address onto the request as a header — would put a
    /// spoofable-*looking* field on every request for the sake of one reader.
    static SOURCE: String;
}

/// Refuse now if this source has already spent its failure budget. Runs **before** any credential
/// comparison; see the module docs for why that ordering is load-bearing.
pub(crate) fn guard(st: &AppState) -> Result<(), ApiError> {
    SOURCE
        .try_with(|s| st.auth_throttle.check(s))
        .unwrap_or(Ok(()))
}

/// A credential was accepted: wipe this source's failure record.
pub(crate) fn record_success(st: &AppState) {
    let _ = SOURCE.try_with(|s| st.auth_throttle.success(s));
}

/// A credential was rejected. Only ever called for an actual 401 — a store outage must not count, or
/// an unreachable database would lock every source out of a server that is merely sick.
pub(crate) fn record_failure(st: &AppState) {
    let _ = SOURCE.try_with(|s| st.auth_throttle.failure(s));
}

/// Establish [`SOURCE`] for the request from its socket peer (and, only when configured, a trusted
/// `X-Forwarded-For` hop). Layered over the whole router; it does nothing else.
///
/// No `ConnectInfo` means no source, and no source means no throttling — collapsing unidentifiable
/// callers into one shared bucket would be a lockout vector, not a control. `main` installs
/// `ConnectInfo` on the real server and logs [`AuthThrottle::describe`] at startup, so an operator
/// can see the posture rather than assume it.
pub(crate) async fn source_scope(State(st): State<AppState>, req: Request, next: Next) -> Response {
    match source::of(&req, st.auth_throttle.trusted_hops()) {
        Some(src) => SOURCE.scope(src, next.run(req)).await,
        None => next.run(req).await,
    }
}
