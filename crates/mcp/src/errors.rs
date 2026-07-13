//! Status-aware error translation. The API's failures reach the client as `HTTP {code}: {body}`
//! (see [`crate::client`]); a bare code isn't actionable to an agent, so we prepend a short line that
//! says *what to do about it* while preserving the original body verbatim underneath. Non-HTTP errors
//! (transport failures, our own argument validation) pass through unchanged.

/// Translate a client error string into agent-facing guidance + the preserved original.
pub(crate) fn map_error(raw: &str) -> String {
    match parse_http(raw) {
        // The prompt-registry promotion gate returns 409 with a "promotion blocked: …" body. Lead
        // with a plain-language verdict so an agent reads it as a decision, not a transport failure.
        Some((409, body)) if body.contains("promotion blocked") => format!(
            "error: promotion blocked by the benchmark regression gate — the linked benchmark's latest \
             mean is below its baseline (or it has no scored run yet). Re-run the benchmark and re-check, \
             or promote with force=true to override intentionally.\n\n{body}"
        ),
        Some((code, body)) => match guidance(code) {
            // 429 and other 4xx already carry a human-facing body (the breach details / bad-request
            // message); show it without inventing guidance that might contradict it.
            None => format!("error: {body}"),
            Some(g) => format!("error: {g}\n\n{body}"),
        },
        None => format!("error: {raw}"),
    }
}

/// The one-line remediation for a status class, or `None` when the body speaks for itself.
fn guidance(code: u16) -> Option<&'static str> {
    match code {
        401 | 403 => Some(
            "authentication failed — set LIGHTTRACK_KEY (and an admin key for write tools).",
        ),
        404 => Some("not found — check the id (list it first with the matching list_* tool)."),
        429 => None, // the breach body names the limit that was hit
        c if c >= 500 => {
            Some("LightTrack API error — is the server healthy? (try the /health endpoint).")
        }
        _ => None, // other 4xx: the API's message is already descriptive
    }
}

/// Split `HTTP {code}: {body}` back into its parts. `None` for any other error shape.
fn parse_http(raw: &str) -> Option<(u16, &str)> {
    let rest = raw.strip_prefix("HTTP ")?;
    let (code, body) = rest.split_once(": ")?;
    Some((code.parse().ok()?, body))
}

#[cfg(test)]
mod tests {
    use super::map_error;

    #[test]
    fn auth_codes_get_credential_guidance() {
        for code in [401, 403] {
            let out = map_error(&format!("HTTP {code}: {{\"error\":\"nope\"}}"));
            assert!(out.contains("authentication failed"), "{out}");
            assert!(out.contains("LIGHTTRACK_KEY"));
            assert!(out.contains("{\"error\":\"nope\"}"), "body preserved: {out}");
        }
    }

    #[test]
    fn not_found_points_at_list_tools() {
        let out = map_error("HTTP 404: {\"error\":{\"message\":\"limit rule 'x' not found\"}}");
        assert!(out.contains("not found — check the id"));
        assert!(out.contains("list_* tool"));
        assert!(out.contains("limit rule 'x' not found"));
    }

    #[test]
    fn rate_limit_surfaces_the_breach_body_without_generic_guidance() {
        let body = "{\"error\":{\"message\":\"ingest blocked: over daily cost limit\"}}";
        let out = map_error(&format!("HTTP 429: {body}"));
        assert!(out.contains("ingest blocked: over daily cost limit"));
        assert!(!out.contains("authentication failed"));
        assert!(!out.contains("server healthy"));
    }

    #[test]
    fn server_errors_ask_about_health() {
        for code in [500, 502, 503] {
            let out = map_error(&format!("HTTP {code}: upstream boom"));
            assert!(out.contains("is the server healthy"), "{out}");
            assert!(out.contains("upstream boom"));
        }
    }

    #[test]
    fn promotion_409_leads_with_a_gate_verdict() {
        let body = "{\"error\":{\"message\":\"promotion blocked: benchmark mean 0.720 regressed below baseline 0.800 (pass force=true to override)\"}}";
        let out = map_error(&format!("HTTP 409: {body}"));
        assert!(out.contains("promotion blocked by the benchmark regression gate"), "{out}");
        assert!(out.contains("force=true"));
        assert!(out.contains("regressed below baseline 0.800"), "original body preserved: {out}");
    }

    #[test]
    fn other_409s_are_not_mistaken_for_the_gate() {
        // e.g. duplicate prompt name — no gate wording, so no gate verdict.
        let out = map_error("HTTP 409: {\"error\":{\"message\":\"prompt 'x' already exists\"}}");
        assert!(out.contains("already exists"));
        assert!(!out.contains("regression gate"), "{out}");
    }

    #[test]
    fn plain_4xx_shows_the_api_message_only() {
        let out = map_error("HTTP 400: {\"error\":{\"message\":\"threshold must be > 0\"}}");
        assert!(out.contains("threshold must be > 0"));
        assert!(!out.contains("authentication failed"));
        assert!(!out.contains("not found"));
    }

    #[test]
    fn non_http_errors_pass_through() {
        assert_eq!(map_error("missing required argument: id"), "error: missing required argument: id");
        assert_eq!(
            map_error("error sending request for url"),
            "error: error sending request for url"
        );
    }
}
