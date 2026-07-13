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
    /// Returned by ingest admission when an enforcing (`throttle`/`block`) limit is breached: the
    /// event is rejected and not recorded so a cooperating client backs off (see
    /// `docs/ARCHITECTURE.md` §7).
    RateLimited,
    /// An unexpected server-side failure (store, serialization, I/O). HTTP 500.
    Internal,
}

impl ErrorCode {
    /// The canonical HTTP status for this code.
    pub(crate) fn status(self) -> StatusCode {
        match self {
            ErrorCode::BadRequest => StatusCode::BAD_REQUEST,
            ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
            ErrorCode::Forbidden => StatusCode::FORBIDDEN,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::Conflict => StatusCode::CONFLICT,
            ErrorCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The stable wire string (snake_case), e.g. `"not_found"`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::Unauthorized => "unauthorized",
            ErrorCode::Forbidden => "forbidden",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Conflict => "conflict",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::Internal => "internal",
        }
    }
}

pub(crate) struct ApiError {
    code: ErrorCode,
    message: String,
}

impl ApiError {
    pub(crate) fn new(code: ErrorCode, m: impl Into<String>) -> Self {
        Self {
            code,
            message: m.into(),
        }
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
        (self.code.status(), body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn code_status_mapping_is_canonical() {
        assert_eq!(ErrorCode::BadRequest.status(), StatusCode::BAD_REQUEST);
        assert_eq!(ErrorCode::Unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(ErrorCode::Forbidden.status(), StatusCode::FORBIDDEN);
        assert_eq!(ErrorCode::NotFound.status(), StatusCode::NOT_FOUND);
        assert_eq!(ErrorCode::Conflict.status(), StatusCode::CONFLICT);
        assert_eq!(ErrorCode::RateLimited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(ErrorCode::Internal.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn code_wire_strings_are_stable() {
        assert_eq!(ErrorCode::BadRequest.as_str(), "bad_request");
        assert_eq!(ErrorCode::Unauthorized.as_str(), "unauthorized");
        assert_eq!(ErrorCode::Forbidden.as_str(), "forbidden");
        assert_eq!(ErrorCode::NotFound.as_str(), "not_found");
        assert_eq!(ErrorCode::Conflict.as_str(), "conflict");
        assert_eq!(ErrorCode::RateLimited.as_str(), "rate_limited");
        assert_eq!(ErrorCode::Internal.as_str(), "internal");
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
