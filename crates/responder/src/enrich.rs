//! Context enrichment: pull the project's recent failing events back from LightTrack so the
//! investigator sees the real request/response shape, not just the one error string from the alert.

use serde_json::Value;

/// Fetch up to `limit` recent events for `project` and format the non-success ones as a compact
/// bullet list. Best-effort: any failure returns a short note instead of erroring the pipeline.
pub(crate) async fn recent_failures(
    client: &reqwest::Client,
    base_url: &str,
    project: &str,
    limit: usize,
) -> String {
    let url = format!("{base_url}/v1/events?project={project}&limit={limit}");
    let events: Vec<Value> = match client.get(&url).send().await {
        Ok(resp) => match resp.json().await {
            Ok(v) => v,
            Err(e) => return format!("(enrichment unavailable: bad response from LightTrack: {e})"),
        },
        Err(e) => return format!("(enrichment unavailable: {e})"),
    };

    let mut lines = Vec::new();
    for ev in &events {
        let status = ev.get("status").and_then(Value::as_str).unwrap_or("success");
        if status == "success" {
            continue;
        }
        let ts = ev.get("ts").and_then(Value::as_str).unwrap_or("?");
        let model = ev.get("model").and_then(Value::as_str).unwrap_or("?");
        let err = ev.get("error").and_then(Value::as_str).unwrap_or("(no message)");
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
