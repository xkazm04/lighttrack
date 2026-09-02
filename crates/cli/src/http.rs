//! Thin HTTP client over the LightTrack API: issue one request, print the response, exit non-zero
//! on an HTTP error.

use std::io::IsTerminal;

use anyhow::Result;
use reqwest::Method;
use serde_json::Value;

use crate::cli::Cli;

/// Wall-clock bound on one request. An unreachable-but-accepting address (a firewall that
/// blackholes, a server wedged mid-response) used to hang the command until the operator killed it,
/// with nothing printed; every verb is one request, so one generous bound covers them all.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub(crate) fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

/// Whether a response should be shown as a rendered table. Only ever true for a *successful* body on
/// an interactive stdout: piped or `--json` output stays raw JSON so scripts keep parsing it.
pub(crate) fn wants_table(json_flag: bool, success: bool, tty: bool) -> bool {
    !json_flag && success && tty
}

/// Turn a response body into the block to print: Markdown when a renderer matches `kind` and the
/// caller asked for a table, pretty JSON when it parses as JSON, and the body verbatim when it does
/// not (an error page or plain-text message must still reach the operator).
pub(crate) fn present(text: &str, kind: &str, table: bool) -> Result<String> {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => {
            let rendered = table.then(|| lighttrack_render::render(kind, &v)).flatten();
            match rendered {
                Some(md) => Ok(md),
                None => Ok(serde_json::to_string_pretty(&v)?),
            }
        }
        Err(_) => Ok(text.to_string()),
    }
}

/// The request URL for `path` under `base`. `LIGHTTRACK_URL=https://host/` is how a URL is usually
/// pasted, and the naive concatenation produced `https://host//v1/...`, which the router answers
/// with a 404 that reads as "no such endpoint" — for every verb, on an otherwise correct setup.
pub(crate) fn url(base: &str, path: &str) -> String {
    format!("{}{path}", base.trim_end_matches('/'))
}

/// Issue one request and print the response, then exit non-zero on HTTP error.
pub(crate) fn call(
    cli: &Cli,
    method: Method,
    path: &str,
    body: Option<Value>,
    kind: &str,
) -> Result<()> {
    let mut req = client().request(method, url(&cli.base, path));
    if let Some(k) = &cli.key {
        req = req.bearer_auth(k);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }

    let resp = req.send()?;
    let status = resp.status();
    let text = resp.text()?;
    let table = wants_table(
        cli.json,
        status.is_success(),
        std::io::stdout().is_terminal(),
    );
    println!("{}", present(&text, kind, table)?);
    if !status.is_success() {
        eprintln!("HTTP {}", status.as_u16());
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pasted base URL usually ends in `/`; the route must not start with `//`.
    #[test]
    fn a_trailing_slash_on_the_base_does_not_double_the_path_separator() {
        assert_eq!(url("http://h:1/", "/v1/events"), "http://h:1/v1/events");
        assert_eq!(url("http://h:1", "/v1/events"), "http://h:1/v1/events");
        assert_eq!(url("https://h/lt//", "/health"), "https://h/lt/health");
    }

    #[test]
    fn table_only_on_an_interactive_success() {
        assert!(wants_table(false, true, true));
        // Piped stdout, `--json`, or an error body each force raw JSON.
        assert!(!wants_table(false, true, false));
        assert!(!wants_table(true, true, true));
        assert!(!wants_table(false, false, true));
    }

    #[test]
    fn json_body_is_pretty_printed_when_not_rendering() {
        let out = present(r#"{"a":1}"#, "list_projects", false).expect("present");
        assert_eq!(out, "{\n  \"a\": 1\n}");
    }

    /// An unknown `kind` has no renderer, so even a table-mode call falls back to pretty JSON
    /// rather than printing nothing.
    #[test]
    fn unknown_kind_falls_back_to_pretty_json() {
        let out = present(r#"{"a":1}"#, "", true).expect("present");
        assert_eq!(out, "{\n  \"a\": 1\n}");
    }

    #[test]
    fn non_json_body_is_passed_through_verbatim() {
        let out = present("upstream timeout", "list_projects", true).expect("present");
        assert_eq!(out, "upstream timeout");
    }

    /// A body a renderer does understand is rendered — the branch that makes `--json` meaningful.
    #[test]
    fn known_kind_renders_a_table() {
        let body =
            r#"[{"id":"p1","name":"demo","enabled":true,"created_at":"2026-01-01T00:00:00Z"}]"#;
        let table = present(body, "list_projects", true).expect("present");
        assert!(table.contains("demo") && !table.starts_with('['));
        // Same body, table off → raw JSON.
        let raw = present(body, "list_projects", false).expect("present");
        assert!(raw.starts_with('['));
    }
}
