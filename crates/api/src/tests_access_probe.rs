//! The probe harness the derived access matrix is built on.
//!
//! Split out of `tests_access_matrix` to keep both files inside the repo's size rule: this one is
//! the *instrument* (how a contract row becomes a real request, and how a router refusal is told
//! apart from a handler's answer), that one is the assertions.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::Utc;
use serde_json::Value;
use tower::ServiceExt; // oneshot

use lighttrack_contract::Endpoint;
use lighttrack_core::{new_id, ApiKey, Project, Redaction, Scope};
use lighttrack_store::{SqliteStore, Store};

use crate::auth;

/// Substituted for every `:segment`. A value no fixture creates, so a positive probe that reaches a
/// handler answers "no such row" rather than mutating anything.
pub(crate) const PROBE: &str = "probe-id";

/// The declared template with every `:segment` replaced by [`PROBE`]. `MatchedPath` hands the layer
/// that template back verbatim, so this is the probe's only transformation.
pub(crate) fn probe_uri(e: &Endpoint) -> String {
    e.path
        .split('/')
        .map(|seg| if seg.starts_with(':') { PROBE } else { seg })
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::from("{}")).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, v)
}

pub(crate) async fn probe(app: &Router, e: &Endpoint, token: Option<&str>) -> (StatusCode, Value) {
    let m = e.method.as_str().to_ascii_uppercase();
    send(app, &m, &probe_uri(e), token).await
}

/// A project and a key carrying exactly `scopes` — `tests_ingest::make_key` always mints the
/// default pair, and the point here is the key that is missing a capability.
pub(crate) fn scoped_key(store: &SqliteStore, project_id: &str, scopes: Vec<Scope>) -> String {
    let now = Utc::now();
    store
        .create_project(&Project {
            id: project_id.into(),
            name: project_id.into(),
            enabled: true,
            redaction: Redaction::None,
            collective_opt_in: false,
            require_trusted_judge: false,
            archived_at: None,
            created_at: now,
        })
        .unwrap();
    let g = auth::generate_key();
    store
        .create_api_key(&ApiKey {
            id: new_id(),
            project_id: project_id.into(),
            name: "probe".into(),
            prefix: g.prefix.clone(),
            key_hash: g.key_hash,
            created_at: now,
            last_used_at: None,
            revoked: false,
            scopes,
            expires_at: None,
        })
        .unwrap();
    g.full_key
}

pub(crate) fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("<none>")
}

/// Was this the **router** refusing the pair, or a handler answering about a row? The default axum
/// fallback answers 404 with an empty body; every handler 404 carries the `ApiError` envelope.
pub(crate) fn router_miss(status: StatusCode, body: &Value) -> bool {
    status == StatusCode::METHOD_NOT_ALLOWED || (status == StatusCode::NOT_FOUND && body.is_null())
}

/// One line of a failure report, so the panic names every offending row rather than the first.
pub(crate) fn note(e: &Endpoint, status: StatusCode, detail: &str) -> String {
    format!("{} {} -> {status} {detail}", e.method.as_str(), e.path)
}

/// The floor every derived test carries: a selection that selected nothing must not pass.
pub(crate) fn floor(n: usize, min: usize, what: &str) {
    assert!(
        n >= min,
        "only {n} {what} rows found — the derivation selected nothing (expected >= {min})"
    );
}
