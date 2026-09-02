//! Alert transport: the actual HTTP post for one channel, and the [`Delivery`] it produces.
//!
//! One function, three arms. Before the ledger there were six `deliver_*` composers, each fanning
//! out to three hard-coded env destinations and logging the outcome to stderr; now the *body* is
//! already assembled ([`super::compose`]) and the *destination* is already chosen
//! ([`super::routing`]), so this is a driver.
//!
//! Every path is best-effort and returns rather than propagates: a down sink is a recorded failed
//! delivery, never a failed ingest. And every path returns a [`Delivery`], so "was anyone actually
//! told" is a stored fact instead of a line in a log nobody kept.

use chrono::Utc;
use lighttrack_core::{Alert, AlertChannel, ChannelKind, Delivery};
use serde_json::json;

use super::{compose, sign, vet, Alerter};

/// Cap on how much of a receiver's response body we read. A webhook endpoint that answers with a
/// megabyte of HTML must not be able to make an alert delivery expensive.
const MAX_RESPONSE_BYTES: usize = 2048;

/// Deliver one alert down one channel and report what happened.
pub(crate) async fn deliver(alerter: &Alerter, c: &AlertChannel, a: &Alert) -> Delivery {
    let status = match c.kind {
        ChannelKind::Webhook => post_webhook(alerter, c, a).await,
        ChannelKind::Ntfy => post_ntfy(alerter, c, a).await,
        ChannelKind::Email => post_resend(alerter, c, a).await,
    };
    let (ok, status) = match status {
        Ok(s) => (true, Some(s)),
        Err(e) => {
            tracing::warn!(
                channel = %c.id, kind = c.kind.as_str(), event = a.kind.as_str(), error = %e,
                "alert delivery failed"
            );
            (false, Some(e))
        }
    };
    Delivery {
        channel_id: c.id.clone(),
        ok,
        status,
        at: Utc::now(),
    }
}

/// POST the composed body, signed when the channel carries a key.
///
/// The signature covers the exact bytes on the wire, so the body is serialized once and both the
/// header and the request are built from that one string — re-serializing would risk a different
/// key order and a signature the receiver cannot verify.
async fn post_webhook(alerter: &Alerter, c: &AlertChannel, a: &Alert) -> Result<String, String> {
    vet::check(&c.target, alerter.config.dev_destinations).await?;
    let body = serde_json::to_string(&a.payload).map_err(|e| e.to_string())?;
    let mut req = alerter
        .http
        .post(&c.target)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.clone());
    if let Some(h) = sign::signature_header(
        c.secret_hash.as_deref(),
        c.prev_secret_hash.as_deref(),
        Utc::now().timestamp(),
        &body,
    ) {
        req = req.header(sign::SIGNATURE_HEADER, h);
    }
    send(req).await
}

async fn post_ntfy(alerter: &Alerter, c: &AlertChannel, a: &Alert) -> Result<String, String> {
    vet::check(&c.target, alerter.config.dev_destinations).await?;
    let req = alerter
        .http
        .post(&c.target)
        .header("Title", compose::subject_of(a))
        .header("Tags", "warning")
        .header("Priority", "high")
        .body(compose::text_of(a).to_string());
    send(req).await
}

/// Send the alert as a plain-text email via Resend's REST API. The channel's `target` is the
/// recipient list; the API key and sender stay env-global, because they are the *account*, not the
/// destination — a per-project channel should not be able to send as someone else's domain.
async fn post_resend(alerter: &Alerter, c: &AlertChannel, a: &Alert) -> Result<String, String> {
    let Some(r) = &alerter.config.resend else {
        return Err("email channel configured but LIGHTTRACK_ALERT_RESEND_KEY is not set".into());
    };
    let to: Vec<&str> = c.target.split(',').map(str::trim).collect();
    let body = json!({
        "from": r.from,
        "to": to,
        "subject": compose::subject_of(a),
        "text": compose::text_of(a),
    });
    let req = alerter
        .http
        .post("https://api.resend.com/emails")
        .bearer_auth(&r.key)
        .json(&body);
    send(req).await
}

/// Send, and reduce the answer to a short status string. A non-2xx is a failure with the code and a
/// capped snippet of the body — enough for an operator to see "401 invalid token" in the ledger,
/// which is the detail that used to live only in stderr.
async fn send(req: reqwest::RequestBuilder) -> Result<String, String> {
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let code = resp.status();
    if code.is_success() {
        return Ok(code.as_u16().to_string());
    }
    let detail = capped_body(resp).await;
    Err(if detail.is_empty() {
        code.as_u16().to_string()
    } else {
        format!("{} {}", code.as_u16(), detail)
    })
}

/// Read at most [`MAX_RESPONSE_BYTES`] of the response, streaming so an oversized body is never
/// fully buffered.
async fn capped_body(mut resp: reqwest::Response) -> String {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < MAX_RESPONSE_BYTES {
        match resp.chunk().await {
            Ok(Some(c)) => buf.extend_from_slice(&c),
            _ => break,
        }
    }
    buf.truncate(MAX_RESPONSE_BYTES);
    String::from_utf8_lossy(&buf).trim().replace('\n', " ")
}
