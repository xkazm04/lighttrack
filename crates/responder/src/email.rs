//! Optional email delivery of a finished diagnosis via Resend, so a report doesn't just sit in a
//! local file. Reuses the API's alert email config (`LIGHTTRACK_ALERT_RESEND_KEY` / `_EMAIL_TO` /
//! `_EMAIL_FROM`) so email is set up once; responder-specific `LIGHTTRACK_RESPONDER_*` vars override.

use std::time::Duration;

use serde_json::json;

pub(crate) struct EmailConfig {
    key: String,
    from: String,
    to: Vec<String>,
}

impl EmailConfig {
    pub(crate) fn from_env() -> Option<Self> {
        let key = or_env("LIGHTTRACK_RESPONDER_RESEND_KEY", "LIGHTTRACK_ALERT_RESEND_KEY")?;
        let to: Vec<String> = or_env("LIGHTTRACK_RESPONDER_EMAIL_TO", "LIGHTTRACK_ALERT_EMAIL_TO")?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if to.is_empty() {
            return None;
        }
        let from = or_env("LIGHTTRACK_RESPONDER_EMAIL_FROM", "LIGHTTRACK_ALERT_EMAIL_FROM")
            .unwrap_or_else(|| "onboarding@resend.dev".to_string());
        Some(EmailConfig { key, from, to })
    }

    pub(crate) fn recipients(&self) -> usize {
        self.to.len()
    }
}

/// Best-effort: send an HTML email with a plain-text fallback. Logs and moves on if Resend rejects it.
pub(crate) async fn send(cfg: &EmailConfig, subject: &str, html: &str, text: &str) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let body = json!({ "from": cfg.from, "to": cfg.to, "subject": subject, "html": html, "text": text });
    match http
        .post("https://api.resend.com/emails")
        .bearer_auth(&cfg.key)
        .json(&body)
        .send()
        .await
    {
        Ok(r) if !r.status().is_success() => {
            let code = r.status();
            let detail = r.text().await.unwrap_or_default();
            eprintln!("[responder] email -> HTTP {code}: {}", detail.trim());
        }
        Err(e) => eprintln!("[responder] email error: {e}"),
        _ => {}
    }
}

fn or_env(specific: &str, shared: &str) -> Option<String> {
    env_opt(specific).or_else(|| env_opt(shared))
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}
