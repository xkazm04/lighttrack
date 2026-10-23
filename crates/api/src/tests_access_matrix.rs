//! The access matrix, **derived** from `lighttrack_contract::endpoints()` rather than listed.
//!
//! What this replaces was a text scan of `main.rs` compared against deduplicated path strings: the
//! method was invisible, prose about a route satisfied it, and *nothing* asserted that an
//! `access: Admin` row actually refuses a project key. Here every assertion is a real request
//! through `crate::build_router` and the population is the contract table, so a row added without a
//! decision about who may call it fails on the commit that adds it. Every test carries a floor — a
//! derived selection that selects nothing must not pass — and the router-miss discriminator is
//! itself seeded and watched go red.

use axum::http::StatusCode;

use lighttrack_contract::{Access, KeyScope};
use lighttrack_core::Scope;

use crate::auth::AuthMode;
use crate::redact::Redactor;
use crate::tests_access_probe::{code, floor, note, probe, router_miss, scoped_key, send};
use crate::tests_ingest::setup;

/// Case 1 — every `Admin` row refuses a project key, whatever scopes it carries.
#[tokio::test]
async fn every_admin_row_refuses_a_project_key() {
    let (state, store) = setup(Redactor::off());
    let key = scoped_key(
        &store,
        "p-all-scopes",
        vec![Scope::Ingest, Scope::Read, Scope::Manage],
    );
    let app = crate::build_router(state);

    let mut n = 0usize;
    let mut bad = Vec::new();
    for e in lighttrack_contract::endpoints().filter(|e| e.access == Access::Admin) {
        n += 1;
        let (status, body) = probe(&app, e, Some(&key)).await;
        if status != StatusCode::FORBIDDEN || code(&body) != "forbidden" {
            bad.push(note(e, status, code(&body)));
        }
    }
    floor(n, 69, "Admin");
    assert!(
        bad.is_empty(),
        "admin rows reachable by a project key:\n{}",
        bad.join("\n")
    );
}

/// Case 2 — a guarded row with no credential is 401, *including* the mutating rows that used to
/// answer 422 first because `Json<T>` was extracted before the handler called `authenticate`.
#[tokio::test]
async fn every_guarded_row_refuses_a_missing_credential() {
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);

    let mut n = 0usize;
    let mut bad = Vec::new();
    for e in lighttrack_contract::endpoints().filter(|e| e.access != Access::Unauthenticated) {
        n += 1;
        let (status, body) = probe(&app, e, None).await;
        if status != StatusCode::UNAUTHORIZED || code(&body) != "unauthorized" {
            bad.push(note(e, status, code(&body)));
        }
    }
    floor(n, 127, "guarded");
    assert!(
        bad.is_empty(),
        "rows answering something other than 401 with no credential:\n{}",
        bad.join("\n")
    );
}

/// Case 3 — a `Key(Read)` row called with an ingest-only key names the scope it wants and the ones
/// the key holds. GUARD: `ensure_scope`'s message shape, unchanged.
#[tokio::test]
async fn a_read_row_names_the_scope_the_key_lacks() {
    let (state, store) = setup(Redactor::off());
    let ingest_only = scoped_key(&store, "p-ingest-only", vec![Scope::Ingest]);
    let app = crate::build_router(state);

    let mut n = 0usize;
    let mut bad = Vec::new();
    for e in lighttrack_contract::endpoints().filter(|e| e.access == Access::Key(KeyScope::Read)) {
        n += 1;
        let (status, body) = probe(&app, e, Some(&ingest_only)).await;
        let msg = body["error"]["message"].as_str().unwrap_or("");
        let shaped = msg.contains("is not scoped for this call: needs 'read', has [ingest]");
        if status != StatusCode::FORBIDDEN || code(&body) != "forbidden" || !shaped {
            bad.push(note(e, status, msg));
        }
    }
    floor(n, 45, "Key(Read)");
    assert!(
        bad.is_empty(),
        "read rows served to an ingest-only key:\n{}",
        bad.join("\n")
    );
}

/// Case 4 — the `Unauthenticated` rows answer something other than 401/403 with no credential, and
/// the two k8s probes — routed in `build_router` but absent from the table until this change
/// declared them, which the unresolved-pair 500 rule would have turned into a restart loop — still
/// answer a keyless 200.
#[tokio::test]
async fn unauthenticated_rows_need_no_credential() {
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);

    let mut n = 0usize;
    for e in lighttrack_contract::endpoints().filter(|e| e.access == Access::Unauthenticated) {
        n += 1;
        let (status, _) = probe(&app, e, None).await;
        assert!(
            status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN,
            "{} {} is declared Unauthenticated but answered {status}",
            e.method.as_str(),
            e.path
        );
    }
    assert!(n >= 3, "only {n} Unauthenticated rows found");

    for p in ["/health/live", "/health/ready"] {
        assert!(
            lighttrack_contract::endpoints().any(|e| e.path == p),
            "{p} is routed but not declared — the layer's unresolved-pair rule would 500 a k8s probe"
        );
        let (status, _) = send(&app, "GET", p, None).await;
        assert_eq!(status, StatusCode::OK, "{p} must stay a keyless 200");
    }
}

/// Case 5 — the bijection with the method back in it: every row, called with its declared
/// credential, is really served. `route_paths()` dedups, so a declared `GET` wired only as `POST`
/// used to pass.
#[tokio::test]
async fn every_declared_row_is_wired_with_its_declared_method() {
    let (state, store) = setup(Redactor::off());
    let key = scoped_key(
        &store,
        "p-declared",
        vec![Scope::Ingest, Scope::Read, Scope::Manage],
    );
    let app = crate::build_router(state);

    let mut n = 0usize;
    let mut bad = Vec::new();
    for e in lighttrack_contract::endpoints() {
        n += 1;
        let token = match e.access {
            Access::Admin => Some("admin-secret".to_string()),
            Access::Key(_) => Some(key.clone()),
            Access::Unauthenticated => None,
        };
        let (status, body) = probe(&app, e, token.as_deref()).await;
        if router_miss(status, &body) {
            bad.push(format!("{} {} -> {status}", e.method.as_str(), e.path));
        }
    }
    floor(n, 130, "contract");
    assert!(
        bad.is_empty(),
        "declared rows the router does not serve with that method:\n{}",
        bad.join("\n")
    );
}

/// Case 6 — the instrument, seeded. A `/v1/...` path that exists only as text (a comment, a
/// non-`.route()` literal) is a router miss, where the old source-text pin was satisfied by the
/// text alone.
#[tokio::test]
async fn an_unwired_v1_path_is_a_router_miss() {
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);
    let (status, body) = send(&app, "GET", "/v1/comment-only-route", None).await;
    assert!(
        router_miss(status, &body),
        "an unwired path answered {status} — the router-miss discriminator cannot fail"
    );
    // ...and a real route is not a miss, so the discriminator is not simply always true.
    let (status, body) = send(&app, "GET", "/health", None).await;
    assert!(
        !router_miss(status, &body),
        "/health reported as a router miss"
    );
}

/// Case 7 — the floor. Every `Access` variant must be represented, so a table that lost a class of
/// row (or a filter that stopped matching) fails loudly instead of reporting nothing.
#[test]
fn the_matrix_covers_every_access_variant() {
    let mut admin = 0;
    let mut public = 0;
    let mut by_scope = [0usize; 3];
    let mut total = 0;
    for e in lighttrack_contract::endpoints() {
        total += 1;
        match e.access {
            Access::Admin => admin += 1,
            Access::Unauthenticated => public += 1,
            Access::Key(KeyScope::Ingest) => by_scope[0] += 1,
            Access::Key(KeyScope::Read) => by_scope[1] += 1,
            Access::Key(KeyScope::Manage) => by_scope[2] += 1,
        }
    }
    assert!(total >= 130, "the contract resolved only {total} rows");
    assert!(admin > 0, "no Admin rows");
    assert!(public > 0, "no Unauthenticated rows");
    for (i, name) in ["ingest", "read", "manage"].iter().enumerate() {
        assert!(by_scope[i] > 0, "no Key({name}) rows");
    }
}

/// Case 8 GUARD — the layer decides *who may call*, never *what exists*: dev mode still answers
/// keylessly (`tests_dev_mode` / `tests_scope` carry the 404-not-403 half).
#[tokio::test]
async fn dev_mode_still_answers_without_a_credential() {
    let (mut state, _store) = setup(Redactor::off());
    state.auth_mode = AuthMode::Dev;
    state.admin_key = None;
    let app = crate::build_router(state);

    for e in lighttrack_contract::endpoints()
        .filter(|e| e.access == Access::Admin && e.method.is_read())
        .take(12)
    {
        let (status, body) = probe(&app, e, None).await;
        assert!(
            status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN,
            "dev mode refused {} {} with {status} {}",
            e.method.as_str(),
            e.path,
            code(&body)
        );
    }
}
