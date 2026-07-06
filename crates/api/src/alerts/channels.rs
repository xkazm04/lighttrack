//! Alert transport: the actual HTTP posts to webhook / ntfy / Resend (email), and the per-alert-type
//! `deliver_*` composers that the orchestrator (`super`) spawns off the request path. Free functions
//! over `&AlertConfig` + `&reqwest::Client` with no state of their own, mirroring the store's
//! per-domain function split. Every path is best-effort: a down sink logs to stderr, never panics.

use lighttrack_core::{LimitStatus, RelayTask};
use reqwest::Client;
use serde_json::{json, Value};

use super::{AlertConfig, ErrorSpike};
use crate::forecast::ForecastAlert;

pub(super) async fn deliver_breaches(cfg: &AlertConfig, http: &Client, breaches: &[LimitStatus]) {
    for b in breaches {
        let msg = breach_message(b);
        post_webhook(cfg, http, "limit_breach", &msg, json!({ "breach": b })).await;
        post_ntfy(cfg, http, "LightTrack limit breach", &msg).await;
        post_resend(cfg, http, &format!("LightTrack: limit breach in '{}'", b.project_id), &msg).await;
    }
}

pub(super) async fn deliver_forecast(cfg: &AlertConfig, http: &Client, alerts: &[ForecastAlert]) {
    for a in alerts {
        post_webhook(cfg, http, "forecast_alert", &a.message, json!({ "forecast": a })).await;
        post_ntfy(cfg, http, "LightTrack forecast", &a.message).await;
        post_resend(cfg, http, "LightTrack: spend forecast alert", &a.message).await;
    }
}

pub(super) async fn deliver_relay_dead(cfg: &AlertConfig, http: &Client, tasks: &[RelayTask]) {
    for t in tasks {
        let msg = format!(
            "LightTrack alert: relay task '{}' ({}) in project '{}' dead-lettered after {} \
             attempt(s) — {}",
            t.id,
            t.action_type,
            t.project_id,
            t.attempts,
            t.error.as_deref().unwrap_or("no error recorded"),
        );
        // Not the full row: payload/result can be large and may carry app data.
        let trimmed = json!({ "task": {
            "id": t.id, "project_id": t.project_id, "action_type": t.action_type,
            "source": t.source, "attempts": t.attempts, "error": t.error,
        }});
        post_webhook(cfg, http, "relay_task_dead", &msg, trimmed).await;
        post_ntfy(cfg, http, "LightTrack relay task dead", &msg).await;
        post_resend(cfg, http, &format!("LightTrack: relay task dead in '{}'", t.project_id), &msg).await;
    }
}

pub(super) async fn deliver_error_spike(cfg: &AlertConfig, http: &Client, s: &ErrorSpike) {
    let mins = (s.window_secs / 60).max(1);
    let sample = s.error.as_deref().unwrap_or("(no error message)");
    let msg = format!(
        "LightTrack alert: project '{}' logged {} failed call(s) within {}m. \
         Latest: {} on model '{}'. Sample error: {}",
        s.project_id, s.count, mins, s.status, s.model, sample
    );
    let extra = json!({ "spike": {
        "project_id": s.project_id, "count": s.count, "window_secs": s.window_secs,
        "model": s.model, "status": s.status, "error": s.error,
    }});
    post_webhook(cfg, http, "error_spike", &msg, extra).await;
    post_ntfy(cfg, http, "LightTrack error spike", &msg).await;
    post_resend(cfg, http, &format!("LightTrack: error spike in '{}'", s.project_id), &msg).await;
}

fn breach_message(b: &LimitStatus) -> String {
    format!(
        "LightTrack alert: project '{}' breached {:?}/{:?} limit — current {:.4} >= threshold {:.4} \
         ({:.0}% of limit), action={:?}",
        b.project_id, b.metric, b.window, b.current, b.threshold, b.ratio * 100.0, b.action
    )
}

/// POST a JSON body to the configured webhook: `text` (Slack) + `content` (Discord) + whatever
/// structured fields `extra` carries (custom receivers). No-op when no webhook is configured.
async fn post_webhook(cfg: &AlertConfig, http: &Client, event: &str, msg: &str, extra: Value) {
    let Some(url) = &cfg.webhook else { return };
    let mut body = json!({ "event": event, "text": msg, "content": msg });
    if let (Some(obj), Some(add)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in add {
            obj.insert(k.clone(), v.clone());
        }
    }
    match http.post(url).json(&body).send().await {
        Ok(r) if !r.status().is_success() => eprintln!("[alert] {event} webhook -> HTTP {}", r.status()),
        Err(e) => eprintln!("[alert] {event} webhook error: {e}"),
        _ => {}
    }
}

async fn post_ntfy(cfg: &AlertConfig, http: &Client, title: &str, msg: &str) {
    let Some(url) = &cfg.ntfy else { return };
    let req = http
        .post(url)
        .header("Title", title)
        .header("Tags", "warning")
        .header("Priority", "high")
        .body(msg.to_string());
    match req.send().await {
        Ok(r) if !r.status().is_success() => eprintln!("[alert] ntfy -> HTTP {}", r.status()),
        Err(e) => eprintln!("[alert] ntfy error: {e}"),
        _ => {}
    }
}

/// Send the alert as a plain-text email via Resend's REST API. No-op when Resend isn't configured.
async fn post_resend(cfg: &AlertConfig, http: &Client, subject: &str, text: &str) {
    let Some(r) = &cfg.resend else { return };
    let body = json!({ "from": r.from, "to": r.to, "subject": subject, "text": text });
    match http
        .post("https://api.resend.com/emails")
        .bearer_auth(&r.key)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if !resp.status().is_success() => {
            let code = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            eprintln!("[alert] resend -> HTTP {code}: {}", detail.trim());
        }
        Err(e) => eprintln!("[alert] resend error: {e}"),
        _ => {}
    }
}
