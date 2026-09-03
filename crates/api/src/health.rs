//! One process, two answers — plus the operator-facing rollup.
//!
//! Until this module existed there was one handler, `async fn health() -> "ok"`, and the chart
//! pointed **both** probes at it. That is not redundancy, it is one wrong answer: a constant can
//! only ever report that axum is bound, so the readiness probe was a liveness probe wearing a
//! readiness name and the pod joined the Service the moment the port opened — cold store,
//! unfinished migration, severed connection and all.
//!
//! The rule the split follows is that **each consumer gets the answer shaped to what it will do**:
//!
//! * `GET /health/live` — what a *restarter* reads. It observes nothing outside the process. A
//!   process that can accept a connection and route a request is alive; that is the whole claim.
//!   Wiring a dependency check to the kubelet's restart trigger converts a slow store into a
//!   restart loop, and the restart makes the store slower — the opposite remedy to the one the
//!   situation wants.
//! * `GET /health/ready` — what a *router* reads. It observes the real store ([`Store::ping`]), so
//!   a pod that is up and cannot serve leaves the Service instead of collecting 5xx on behalf of
//!   clients. Removing from the Service is the correct remedy for "temporarily unable"; restarting
//!   is not.
//! * `GET /health` — unchanged in shape for the operator: `ok` and 200 when everything is green,
//!   and when it is not, 503 with the member that is red **named**. `deploy/README.md`,
//!   `scripts/smoke.sh`, `deploy/cloudrun/deploy.sh` and the Docker `HEALTHCHECK` all curl it and
//!   all still get exactly `ok`.
//!
//! **Three states, two of which share an HTTP status.** A member is `Up`, `Down` (observed
//! unusable) or `Unknown` (the probe itself could not run — a join failure, a task that never got
//! scheduled). Collapsing "could not determine" into either side fails in opposite directions, so
//! it is kept as its own verdict and rendered as its own word. The *status code* is necessarily
//! two-state, because the kubelet's question is two-state; the body carries the truth the status
//! cannot. `Unknown` is not served as ready: an instance that cannot even establish whether it can
//! serve is not one to route traffic at.
//!
//! **Not cached, deliberately.** `probe-caching` governs TTL'd health records and nothing here
//! needs it yet: the probe is one indexed point lookup at 6 requests/minute per pod. If `/health`
//! ever becomes an unauthenticated cost surface that matters, the answer is a cached record with a
//! stated TTL — not a flag set at boot, which would reintroduce exactly the false green this
//! module exists to remove.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use crate::state::AppState;

/// What one observed member of the composite is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Observed, and answering.
    Up,
    /// Observed, and not answering. The payload is why.
    Down(String),
    /// Not observed — the probe could not be run at all. Distinct from `Down` because the
    /// remedies differ and because reporting a guess as an observation is how a health surface
    /// stops being one.
    Unknown(String),
}

/// One dependency, and what was observed about it.
#[derive(Debug, Clone)]
pub(crate) struct Member {
    pub(crate) name: &'static str,
    pub(crate) verdict: Verdict,
}

impl Member {
    fn is_up(&self) -> bool {
        self.verdict == Verdict::Up
    }

    /// `store: down (connection reset)` — the member named, with the reason it is not up.
    fn render(&self) -> String {
        match &self.verdict {
            Verdict::Up => format!("{}: up", self.name),
            Verdict::Down(why) => format!("{}: down ({why})", self.name),
            Verdict::Unknown(why) => format!("{}: unknown ({why})", self.name),
        }
    }
}

/// Observe the store with one real round-trip, on the blocking pool like every other store call.
///
/// A `spawn_blocking` join failure is `Unknown`, not `Down`: the store was never asked.
async fn store_member(st: &AppState) -> Member {
    let store = st.store.clone();
    let verdict = match tokio::task::spawn_blocking(move || store.ping()).await {
        Ok(Ok(())) => Verdict::Up,
        Ok(Err(e)) => Verdict::Down(e.to_string()),
        Err(e) => Verdict::Unknown(format!("the probe task did not run: {e}")),
    };
    Member {
        name: "store",
        verdict,
    }
}

/// Everything `/health` and `/health/ready` observe. One member today; the list is what makes
/// adding a second one a data change rather than a rewrite of two handlers.
async fn members(st: &AppState) -> Vec<Member> {
    vec![store_member(st).await]
}

/// The readiness answer: 200 only when every member is `Up`.
pub(crate) fn render_ready(members: &[Member]) -> (StatusCode, String) {
    let bad: Vec<String> = members
        .iter()
        .filter(|m| !m.is_up())
        .map(Member::render)
        .collect();
    if bad.is_empty() {
        (StatusCode::OK, "ready".to_string())
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("not ready\n{}\n", bad.join("\n")),
        )
    }
}

/// The operator rollup. Green is the literal `ok` the deploy docs, the smoke script and the Docker
/// `HEALTHCHECK` have always compared against; red names its failing members instead of laundering
/// them into a status code nobody can act on.
pub(crate) fn render_composite(members: &[Member]) -> (StatusCode, Value) {
    // JSON because `scripts/smoke.sh` and the container healthcheck read the `status` field ("ok"
    // on green) and the deployment's declared surfaces ride along on the one endpoint every operator
    // already curls. A red rollup still names its member and the reason in `members`.
    let rendered: Vec<String> = members.iter().map(Member::render).collect();
    if members.iter().all(Member::is_up) {
        (
            StatusCode::OK,
            json!({ "status": "ok", "members": rendered }),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "status": "degraded", "members": rendered }),
        )
    }
}

/// `GET /health/live` — the restarter's endpoint.
///
/// It takes no state **by construction**, which is the enforcement: a future edit that wants to
/// observe a dependency here has to add the parameter, and that is the moment to remember that a
/// red here restarts the pod.
pub(crate) async fn live() -> &'static str {
    "live"
}

/// `GET /health/ready` — the router's endpoint.
pub(crate) async fn ready(State(st): State<AppState>) -> (StatusCode, String) {
    render_ready(&members(&st).await)
}

/// `GET /health` — the operator's rollup.
pub(crate) async fn composite(State(st): State<AppState>) -> (StatusCode, Json<Value>) {
    let (code, mut body) = render_composite(&members(&st).await);
    let caps = st.store.capabilities();
    body["backend"] = Value::String(caps.backend.to_string());
    body["capabilities"] = serde_json::to_value(crate::capabilities::CapabilitiesBody::from(caps))
        .unwrap_or(Value::Null);
    (code, Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(name: &'static str, verdict: Verdict) -> Member {
        Member { name, verdict }
    }

    #[test]
    fn green_composite_is_still_the_literal_ok_every_deploy_surface_compares_against() {
        let (status, body) = render_composite(&[m("store", Verdict::Up)]);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok", "scripts/smoke.sh reads this field");
    }

    #[test]
    fn a_red_composite_names_the_member_and_the_reason() {
        let (status, body) = render_composite(&[m("store", Verdict::Down("pool is gone".into()))]);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let body = body.to_string();
        assert!(body.contains("store: down (pool is gone)"), "body: {body}");
    }

    #[test]
    fn could_not_determine_is_rendered_as_itself_not_as_down() {
        let (status, body) = render_ready(&[m("store", Verdict::Unknown("task cancelled".into()))]);
        // Two states share a status code because the kubelet's question is two-state, and an
        // instance that cannot establish whether it can serve is not one to route traffic at. The
        // body is where the third state survives.
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            body.contains("store: unknown (task cancelled)"),
            "body: {body}"
        );
        assert!(
            !body.contains("down"),
            "an unknown must not be reported as an observation"
        );
    }

    #[test]
    fn readiness_needs_every_member_up() {
        let all_up = [m("store", Verdict::Up)];
        assert_eq!(render_ready(&all_up).0, StatusCode::OK);
        let one_down = [
            m("store", Verdict::Up),
            m("prices", Verdict::Down("empty".into())),
        ];
        assert_eq!(render_ready(&one_down).0, StatusCode::SERVICE_UNAVAILABLE);
    }
}
