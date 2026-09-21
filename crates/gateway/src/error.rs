//! Errors in the shape an OpenAI SDK already knows how to read:
//! `{"error": {"message", "type", "code"}}`, plus a `lighttrack` block when there is more to say.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

#[derive(Debug)]
pub struct GatewayError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub detail: Option<Value>,
    pub retry_after_secs: Option<u64>,
}

impl GatewayError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> GatewayError {
        GatewayError {
            status,
            code,
            message: message.into(),
            detail: None,
            retry_after_secs: None,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> GatewayError {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    pub fn unsupported(message: impl Into<String>) -> GatewayError {
        Self::new(StatusCode::BAD_REQUEST, "unsupported", message)
    }

    pub fn model_not_found(message: impl Into<String>) -> GatewayError {
        Self::new(StatusCode::NOT_FOUND, "model_not_found", message)
    }

    /// The project's own cap said no *before* any seat was spent.
    pub fn limit_blocked(detail: Value) -> GatewayError {
        GatewayError {
            detail: Some(detail),
            retry_after_secs: Some(60),
            ..Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "a LightTrack limit rule is enforcing on this project; the call was refused \
                 before any provider was spent",
            )
        }
    }

    /// Every seat in the chain failed. 503 rather than 502 when the failure is a seat on hold,
    /// since the honest advice is "wait", and the header says how long.
    pub fn chain_exhausted(attempts: Vec<Value>, retry_after_secs: Option<u64>) -> GatewayError {
        let status = if retry_after_secs.is_some() {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::BAD_GATEWAY
        };
        GatewayError {
            detail: Some(json!({ "attempts": attempts })),
            retry_after_secs,
            ..Self::new(
                status,
                "upstream_failed",
                "no target in the route's chain answered; see lighttrack.attempts",
            )
        }
    }

    pub fn internal(message: impl Into<String>) -> GatewayError {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let kind = match self.status {
            StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
            s if s.is_server_error() => "server_error",
            _ => "invalid_request_error",
        };
        let mut body = json!({
            "error": { "message": self.message, "type": kind, "code": self.code }
        });
        if let Some(d) = self.detail {
            body["lighttrack"] = d;
        }
        let mut resp = (self.status, axum::Json(body)).into_response();
        if let Some(secs) = self.retry_after_secs {
            if let Ok(v) = header::HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        resp
    }
}
