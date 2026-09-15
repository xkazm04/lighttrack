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
    // `alert_id` is added here rather than in `compose`, because the row's own `id` column is the
    // same fact and duplicating it in the stored payload would be two places to keep in step. On the
    // wire it is what lets a receiver answer back: the responder POSTs its diagnosis to
    // `/v1/alerts/<alert_id>/resolution`, which is what closes the loop.
    let mut payload = a.payload.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("alert_id".into(), serde_json::Value::String(a.id.clone()));
    }
    let body = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
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
    let resp = req.send().await.map_err(|e| e.without_url().to_string())?;
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
            Ok(Some(c)) => {
                let remaining = MAX_RESPONSE_BYTES - buf.len();
                buf.extend_from_slice(&c[..c.len().min(remaining)]);
            }
            _ => break,
        }
    }
    response_detail(&buf)
}

fn response_detail(buf: &[u8]) -> String {
    let decoded = String::from_utf8_lossy(buf);
    let mut out = String::with_capacity(decoded.len().min(MAX_RESPONSE_BYTES));
    for ch in decoded.trim().chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if out.len() + ch.len_utf8() > MAX_RESPONSE_BYTES {
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn transport_errors_do_not_expose_destination_secrets() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            drop(socket);
        });

        let secret = "query-secret-must-not-enter-the-ledger";
        let request = reqwest::Client::new()
            .post(format!("http://{addr}/hook?token={secret}"))
            .body("alert");
        let error = send(request).await.unwrap_err();

        assert!(
            !error.contains(secret),
            "transport error leaked URL: {error}"
        );
    }

    async fn failed_response(body: Vec<u8>) -> reqwest::Response {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 512];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..n]);
            }
            let head = format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        });
        reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn receiver_detail_is_one_control_free_line() {
        let response = failed_response(b"first\rsecond\t\x1b[31m\nthird".to_vec()).await;
        let detail = capped_body(response).await;

        assert!(
            !detail.chars().any(char::is_control),
            "receiver controls reached the delivery record: {detail:?}"
        );
    }

    #[tokio::test]
    async fn receiver_detail_stays_within_the_byte_cap_after_utf8_repair() {
        let response = failed_response(vec![0xff; MAX_RESPONSE_BYTES]).await;
        let detail = capped_body(response).await;

        assert!(
            detail.len() <= MAX_RESPONSE_BYTES,
            "{}-byte detail exceeded the {MAX_RESPONSE_BYTES}-byte cap",
            detail.len()
        );
    }
}
