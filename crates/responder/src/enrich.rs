//! Context enrichment: pull the project's recent failing events back from LightTrack so the
//! investigator sees the real request/response shape, not just the one error string from the alert.
//!
//! Both reads carry the configured bearer token. They used to send none, so on any deployment with
//! auth enforced every read 401'd, every investigation ran on "(enrichment unavailable)", and the
//! model was handed the alert's single error string as its entire evidence — a paid run producing a
//! guess. The failure was invisible because enrichment is best-effort by design.

use serde_json::Value;

/// Attach the bearer token when one is configured. A missing token is not an error here — a
/// dev-mode LightTrack accepts unauthenticated reads — but it is the reason enrichment silently
/// degrades, so the caller's note says so.
fn authed(req: reqwest::RequestBuilder, api_key: Option<&str>) -> reqwest::RequestBuilder {
    match api_key {
        Some(k) => req.bearer_auth(k),
        None => req,
    }
}

/// Fetch up to `limit` recent events for `project` and format the non-success ones as a compact
/// bullet list. Best-effort: any failure returns a short note instead of erroring the pipeline.
pub(crate) async fn recent_failures(
    client: &reqwest::Client,
    base_url: &str,
    project: &str,
    limit: usize,
    api_key: Option<&str>,
) -> String {
    // Ask for the failures, not the newest N of everything. A busy project's last 20 events are
    // mostly successes even mid-spike, so the client-side filter below used to hand the model
    // "(no recent failing events found)" for exactly the incident it was investigating. The server
    // filters on `status`; `error` is the class the classifier already decided this spike is.
    let url = format!("{base_url}/v1/events?project={project}&status=error&limit={limit}");
    let events: Vec<Value> = match authed(client.get(&url), api_key).send().await {
        Ok(resp) => match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return format!("(enrichment unavailable: bad response from LightTrack: {e})")
            }
        },
        Err(e) => return format!("(enrichment unavailable: {e})"),
    };

    let mut lines = Vec::new();
    for ev in &events {
        let status = ev
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("success");
        if status == "success" {
            continue;
        }
        let ts = ev.get("ts").and_then(Value::as_str).unwrap_or("?");
        let model = ev.get("model").and_then(Value::as_str).unwrap_or("?");
        let err = ev
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("(no message)");
        lines.push(format!("- [{ts}] {model} {status}: {err}"));
        if lines.len() >= 10 {
            break;
        }
    }
    if lines.is_empty() {
        "(no recent failing events found in LightTrack)".to_string()
    } else {
        lines.join("\n")
    }
}

/// Fetch recent judge scores for `project` (optionally filtered to one `rubric`) and format them with
/// the judge's reasoning, so the quality-regression investigator sees *why* scores fell. Best-effort.
pub(crate) async fn recent_scores(
    client: &reqwest::Client,
    base_url: &str,
    project: &str,
    rubric: Option<&str>,
    limit: usize,
    api_key: Option<&str>,
) -> String {
    let url = format!("{base_url}/v1/scores?project={project}&limit={limit}");
    let scores: Vec<Value> = match authed(client.get(&url), api_key).send().await {
        Ok(resp) => match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return format!("(enrichment unavailable: bad response from LightTrack: {e})")
            }
        },
        Err(e) => return format!("(enrichment unavailable: {e})"),
    };

    let mut lines = Vec::new();
    for sc in &scores {
        let r = sc.get("rubric").and_then(Value::as_str).unwrap_or("?");
        if let Some(want) = rubric {
            if r != want {
                continue;
            }
        }
        let v = sc.get("value").and_then(Value::as_f64).unwrap_or(0.0);
        let m = sc.get("max").and_then(Value::as_f64).unwrap_or(1.0);
        let created = sc.get("created_at").and_then(Value::as_str).unwrap_or("?");
        let reason = sc.get("reasoning").and_then(Value::as_str).unwrap_or("");
        lines.push(format!("- [{created}] {r} {v}/{m}: {reason}"));
        if lines.len() >= 12 {
            break;
        }
    }
    if lines.is_empty() {
        "(no recent scores found in LightTrack)".to_string()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The read must ask the server for failures. Served from a one-shot local socket so the
    /// assertion is on the request line that actually went over the wire, not on a format string.
    #[tokio::test]
    async fn the_failure_read_filters_on_status_server_side() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            let body = r#"[{"status":"error","ts":"t","model":"m","error":"boom"}]"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        let ctx = recent_failures(
            &reqwest::Client::new(),
            &format!("http://{addr}"),
            "p",
            20,
            Some("k"),
        )
        .await;
        let request = seen.await.unwrap();
        let line = request.lines().next().unwrap_or_default();
        assert!(
            line.contains("status=error"),
            "the failures must be selected by the server: {line}"
        );
        assert!(
            request.contains("authorization: Bearer k")
                || request.contains("Authorization: Bearer k")
        );
        assert!(ctx.contains("boom"), "{ctx}");
    }
}
