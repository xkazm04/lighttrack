//! API error type — maps store/internal failures to a stable, machine-readable HTTP envelope.
//!
//! Every error response is `{"error":{"code":"<code>","message":"..."}}`. The `code` is a stable,
//! documented identifier (see [`ErrorCode`]) so consumers (the CLI, MCP server, external SDKs) can
//! branch on the error type programmatically instead of string-matching human messages. The
//! `message` is human-facing prose that may change wording without notice — never parse it.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use lighttrack_store::StoreError;

/// Stable, machine-readable error codes returned in the `error.code` field.
///
/// These are part of the public API contract: the wire strings (snake_case, via `as_str`) are
/// frozen — consumers may `switch`/`match` on them. New variants may be added over time, but
/// existing ones never change meaning or spelling. Each code has one canonical HTTP status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorCode {
    /// Malformed or semantically invalid request (validation failure). HTTP 400.
    BadRequest,
    /// An ingested event's client `ts` is further in the **past** than the configured skew window
    /// allows. HTTP 400. Split out of `bad_request` so an SDK can react specifically (drop the stale
    /// buffer, or widen `LIGHTTRACK_MAX_TS_SKEW_PAST_SECS`) instead of retrying a payload that will
    /// never be accepted.
    TsTooOld,
    /// An ingested event's client `ts` is further in the **future** than the configured skew window
    /// allows — nearly always a wrong client clock. HTTP 400.
    TsTooNew,
    /// Missing or invalid credentials. HTTP 401.
    Unauthorized,
    /// Authenticated, but not permitted to act on the resource. HTTP 403.
    Forbidden,
    /// The referenced resource does not exist. HTTP 404.
    NotFound,
    /// The request conflicts with current state (duplicate, frozen dataset, gated regression). HTTP 409.
    Conflict,
    /// A usage/ingest limit has been exceeded. HTTP 429.
    ///
    /// Returned by ingest admission in two cases, both carrying a `Retry-After`: an enforcing
    /// (`throttle`/`block`) limit is **breached**, or a `throttle` rule is **shedding** a share of
    /// traffic on its approach to the threshold. Either way the event is not recorded, so a
    /// cooperating client backs off (see `docs/ARCHITECTURE.md` §7 / §7c). The response body's
    /// message distinguishes the two; the shed case is transient and the retry hint is seconds.
    RateLimited,
    /// The server is shedding load: too many ingest requests are already in flight, so this one was
    /// refused immediately rather than queued. HTTP 503, with `Retry-After`.
    ///
    /// **Never confuse this with `rate_limited`.** `rate_limited` (429) means the *caller* exceeded a
    /// configured usage budget and the event was deliberately not recorded; retrying is pointless
    /// until the window rolls. `overloaded` means the *server* is momentarily saturated and the very
    /// same request will succeed shortly. See `shed.rs`.
    Overloaded,
    /// An ingest request outlived the server's deadline and was cut. HTTP 504.
    ///
    /// The write may or may not have landed (the store call is not cancelled when the response
    /// future is dropped) — resending the same event id is the safe way to find out: a replay is
    /// acknowledged, never double-counted.
    Timeout,
    /// An unexpected server-side failure (store, serialization, I/O). HTTP 500.
    Internal,
    /// The configured store backend has not ported the capability behind this endpoint. HTTP 501.
    ///
    /// Distinct from `internal` so a client (or an operator reading logs) can tell a permanent
    /// capability gap — "this deploy's backend doesn't do traces" — from a transient outage, and
    /// never confuses it with an empty-but-authoritative result.
    Unsupported,
}

impl ErrorCode {
    /// The canonical HTTP status for this code.
    pub(crate) fn status(self) -> StatusCode {
        match self {
            ErrorCode::BadRequest | ErrorCode::TsTooOld | ErrorCode::TsTooNew => {
                StatusCode::BAD_REQUEST
            }
            ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
            ErrorCode::Forbidden => StatusCode::FORBIDDEN,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::Conflict => StatusCode::CONFLICT,
            ErrorCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::Overloaded => StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Timeout => StatusCode::GATEWAY_TIMEOUT,
            ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Unsupported => StatusCode::NOT_IMPLEMENTED,
        }
    }

    /// The stable wire string (snake_case), e.g. `"not_found"`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::TsTooOld => "ts_too_old",
            ErrorCode::TsTooNew => "ts_too_new",
            ErrorCode::Unauthorized => "unauthorized",
            ErrorCode::Forbidden => "forbidden",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Conflict => "conflict",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::Overloaded => "overloaded",
            ErrorCode::Timeout => "timeout",
            ErrorCode::Internal => "internal",
            ErrorCode::Unsupported => "unsupported",
        }
    }
}

pub(crate) struct ApiError {
    code: ErrorCode,
    message: String,
    /// Seconds to advertise in a `Retry-After` response header. Set on limit rejections so a
    /// cooperating client has a schedule to honor instead of guessing (a graduated throttle asks for
    /// a short pause, a hard cap for the wait until its window ages out).
    retry_after: Option<u64>,
}

impl ApiError {
    pub(crate) fn new(code: ErrorCode, m: impl Into<String>) -> Self {
        Self {
            code,
            message: m.into(),
            retry_after: None,
        }
    }

    /// Attach a `Retry-After` (seconds) to this error response.
    pub(crate) fn retry_after(mut self, secs: Option<u64>) -> Self {
        self.retry_after = secs;
        self
    }
    pub(crate) fn internal(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, m)
    }
    pub(crate) fn bad_request(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, m)
    }
    pub(crate) fn unauthorized(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unauthorized, m)
    }
    pub(crate) fn forbidden(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Forbidden, m)
    }
    pub(crate) fn not_found(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, m)
    }
    pub(crate) fn conflict(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, m)
    }
    pub(crate) fn rate_limited(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::RateLimited, m)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            // A constraint violation (e.g. a duplicate event id) is a client fault, not a server
            // one: surface it as a stable `conflict`/409 so a client can distinguish an idempotency
            // collision from a real outage instead of seeing an opaque 500.
            StoreError::Conflict(m) => ApiError::conflict(m),
            // A capability the configured backend hasn't ported: 501 `unsupported`, so "this
            // deploy can't answer" is never presented as an empty result or a generic outage.
            e @ StoreError::Unsupported(_) => ApiError::new(ErrorCode::Unsupported, e.to_string()),
            // Every remaining store-layer failure (sqlite/json/io, and the catch-all `Other`) is a
            // server-side fault from a client's perspective: collapse to a single stable `internal`
            // code. Clients must not branch on store internals; the message carries the detail.
            other => ApiError::internal(other.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({
            "error": { "code": self.code.as_str(), "message": self.message }
        }));
        let mut resp = (self.code.status(), body).into_response();
        if let Some(secs) = self.retry_after {
            if let Ok(v) = axum::http::HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut().insert("retry-after", v);
            }
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn unsupported_store_error_maps_to_501() {
        // A backend capability gap must surface as a stable `unsupported`/501 — never a 500
        // (looks like an outage) and never a 200-with-empty (looks like data).
        let api: ApiError = StoreError::Unsupported("traces").into();
        let resp = api.into_response();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], "unsupported");
        assert_eq!(
            v["error"]["message"],
            "traces is not supported by this store backend"
        );
    }

    #[test]
    fn code_status_mapping_is_canonical() {
        assert_eq!(ErrorCode::BadRequest.status(), StatusCode::BAD_REQUEST);
        assert_eq!(ErrorCode::Unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(ErrorCode::Forbidden.status(), StatusCode::FORBIDDEN);
        assert_eq!(ErrorCode::NotFound.status(), StatusCode::NOT_FOUND);
        assert_eq!(ErrorCode::Conflict.status(), StatusCode::CONFLICT);
        assert_eq!(
            ErrorCode::RateLimited.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            ErrorCode::Overloaded.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(ErrorCode::Timeout.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(
            ErrorCode::Internal.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(ErrorCode::Unsupported.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[test]
    fn code_wire_strings_are_stable() {
        assert_eq!(ErrorCode::BadRequest.as_str(), "bad_request");
        assert_eq!(ErrorCode::Unauthorized.as_str(), "unauthorized");
        assert_eq!(ErrorCode::Forbidden.as_str(), "forbidden");
        assert_eq!(ErrorCode::NotFound.as_str(), "not_found");
        assert_eq!(ErrorCode::Conflict.as_str(), "conflict");
        assert_eq!(ErrorCode::RateLimited.as_str(), "rate_limited");
        assert_eq!(ErrorCode::Overloaded.as_str(), "overloaded");
        assert_eq!(ErrorCode::Timeout.as_str(), "timeout");
        assert_eq!(ErrorCode::Internal.as_str(), "internal");
        assert_eq!(ErrorCode::Unsupported.as_str(), "unsupported");
        // Serialize matches as_str (the enum and the wire string can't drift).
        let s = serde_json::to_string(&ErrorCode::NotFound).unwrap();
        assert_eq!(s, "\"not_found\"");
    }

    #[tokio::test]
    async fn response_is_nested_envelope_with_code_and_status() {
        let resp = ApiError::not_found("event 'x' not found").into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], "not_found");
        assert_eq!(v["error"]["message"], "event 'x' not found");
        // The legacy flat `{"error": "<message>"}` shape is gone — `error` is an object now.
        assert!(v["error"].is_object());
    }

    #[tokio::test]
    async fn store_errors_collapse_to_internal() {
        let api: ApiError = StoreError::Other("backend says no".into()).into();
        let resp = api.into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], "internal");
        assert_eq!(v["error"]["message"], "backend says no");
    }

    #[tokio::test]
    async fn conflict_store_error_maps_to_409() {
        // A constraint violation must surface as a stable `conflict`/409, not a 500.
        let api: ApiError = StoreError::Conflict("event 'abc' already exists".into()).into();
        let resp = api.into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], "conflict");
        assert_eq!(v["error"]["message"], "event 'abc' already exists");
    }
}
