//! The responder's read/write side of LightTrack's alert ledger.
//!
//! Two things move out of process memory here.
//!
//! **Admission counts.** The investigate cooldown and the hourly cap lived only in
//! [`crate::breaker`], so a responder restart forgot every investigation it had ever run and the
//! very next retry of a still-firing spike bought another paid Claude run. The ledger already knows:
//! an alert this responder investigated carries a `resolution`, so "have I already looked at this
//! project recently" is a query, not a memory. The in-process breaker stays as the fast path and as
//! the answer when the ledger is unreachable — this only ever makes admission *stricter*.
//!
//! **The diagnosis itself.** It used to exist only as a Markdown file on the responder's local disk
//! (and, optionally, an email). Posting it back as the alert's resolution is what turns a fired
//! alert into a closed loop: `GET /v1/alerts` can say what came of it.

use std::time::Duration;

use serde_json::{json, Value};

/// What the ledger knows about recent investigations, or `None` when it could not be reached.
pub(crate) struct Admission {
    /// This project has a resolved alert inside the cooldown window.
    pub(crate) project_recent: bool,
    /// Resolved alerts across all projects in the last rolling hour.
    pub(crate) hour_count: u32,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

fn get(url: &str, api_key: Option<&str>) -> reqwest::RequestBuilder {
    let req = client().get(url);
    match api_key {
        Some(k) => req.bearer_auth(k),
        None => req,
    }
}

/// Ask the ledger what has already been investigated. Best-effort in the honest direction: an
/// unreachable ledger returns `None` and the caller falls back to its in-process breaker, so this
/// can tighten admission but never loosen it.
pub(crate) async fn admission(
    base_url: &str,
    api_key: Option<&str>,
    project: &str,
    cooldown: Duration,
) -> Option<Admission> {
    let hour = fetch_resolved(base_url, api_key, "1h").await?;
    let cooldown_label = format!("{}m", (cooldown.as_secs() / 60).max(1));
    let recent = fetch_resolved(base_url, api_key, &cooldown_label).await?;
    Some(Admission {
        project_recent: recent
            .iter()
            .any(|a| a.get("project_id").and_then(Value::as_str) == Some(project)),
        hour_count: hour.len() as u32,
    })
}

/// Alerts of the kinds this responder acts on that already carry a resolution — i.e. ones it has
/// already investigated — fired within `since`.
async fn fetch_resolved(base_url: &str, api_key: Option<&str>, since: &str) -> Option<Vec<Value>> {
    let mut out = Vec::new();
    for kind in ["error_spike", "score_drop"] {
        let url = format!("{base_url}/v1/alerts?kind={kind}&since={since}&limit=200");
        let body: Value = get(&url, api_key).send().await.ok()?.json().await.ok()?;
        let rows = body.get("alerts")?.as_array()?;
        out.extend(
            rows.iter()
                .filter(|a| a.get("resolution").is_some())
                .cloned(),
        );
    }
    Some(out)
}

/// POST the diagnosis as the alert's resolution. Admin-keyed (`LIGHTTRACK_API_KEY`); without a key
/// this is a no-op rather than a stream of 401s in the log.
///
/// `cost_usd` stays an `Option`: a run whose cost the CLI never reported must read as *unknown* in
/// the ledger, not as `$0.00` — the second is a number an operator would add up.
pub(crate) async fn post_resolution(
    base_url: &str,
    api_key: Option<&str>,
    alert_id: &str,
    report_path: Option<&str>,
    cost_usd: Option<f64>,
    ok: bool,
    act: Option<&str>,
) {
    let Some(key) = api_key else {
        return;
    };
    let body = json!({
        "responder": "lt-responder",
        "ok": ok,
        "cost_usd": cost_usd,
        "report": report_path,
        "act": act,
        "at": chrono::Utc::now().to_rfc3339(),
    });
    let url = format!("{base_url}/v1/alerts/{alert_id}/resolution");
    match client()
        .post(&url)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            println!("[responder] resolution posted for alert {alert_id}")
        }
        Ok(r) => eprintln!(
            "[responder] could not post resolution for alert {alert_id}: HTTP {}",
            r.status().as_u16()
        ),
        Err(e) => eprintln!("[responder] could not post resolution for alert {alert_id}: {e}"),
    }
}
