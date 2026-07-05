//! Thin HTTP client over the LightTrack API.

use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::cli::Cli;

/// Every outbound runner call (LightTrack API + billing providers) goes through one client bounded
/// by these timeouts, so a black-holed endpoint surfaces as a clean error instead of hanging a run.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard ceiling on a single response body — stops a pathological multi-GB payload from being
/// buffered into memory. Well above any realistic dataset/benchmark response.
const MAX_BODY_BYTES: u64 = 128 * 1024 * 1024;

/// Build the shared blocking client used for every outbound call, with bounded timeouts.
pub(crate) fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("building HTTP client")
}

/// Read a response body with a hard size cap, erroring out if the peer streams past it instead of
/// buffering an unbounded amount into memory.
fn read_bounded(resp: reqwest::blocking::Response, what: &str) -> Result<String> {
    let mut buf = Vec::new();
    resp.take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("reading response from {what}"))?;
    if buf.len() as u64 > MAX_BODY_BYTES {
        anyhow::bail!("{what} response exceeded {MAX_BODY_BYTES}-byte cap");
    }
    String::from_utf8(buf).with_context(|| format!("decoding response from {what} as UTF-8"))
}

/// GET `path` and decode JSON into `T` (bearer auth if a key is set).
pub(crate) fn get<T: serde::de::DeserializeOwned>(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    path: &str,
) -> Result<T> {
    let mut req = http.get(format!("{}{}", cli.base, path));
    if let Some(k) = &cli.key {
        req = req.bearer_auth(k);
    }
    let resp = req.send()?;
    let status = resp.status();
    let text = read_bounded(resp, path)?;
    if !status.is_success() {
        anyhow::bail!("GET {path} -> HTTP {}: {text}", status.as_u16());
    }
    serde_json::from_str(&text).with_context(|| format!("decoding response from {path}"))
}

/// POST `body` to `path`, returning the JSON response (or Null).
pub(crate) fn post(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    path: &str,
    body: &Value,
) -> Result<Value> {
    let mut req = http.post(format!("{}{}", cli.base, path)).json(body);
    if let Some(k) = &cli.key {
        req = req.bearer_auth(k);
    }
    let resp = req.send()?;
    let status = resp.status();
    let text = read_bounded(resp, path)?;
    if !status.is_success() {
        anyhow::bail!("POST {path} -> HTTP {}: {text}", status.as_u16());
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
}
