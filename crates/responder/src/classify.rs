//! Cheap triage: decide whether a failure is a transient/provider problem (no code change can fix it,
//! so don't spend an investigation) or a code-side issue worth pointing Claude Code at.

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// Provider/infra hiccup — rate limit, overload, 5xx, timeout. Skip.
    Transient,
    /// Contract/integration or application-logic failure. Investigate.
    Code,
}

/// Phrase markers of a provider/network transient — matched case-insensitively as substrings
/// against the error message. These are words, not numbers, so a substring match can't collide with
/// unrelated text.
const TRANSIENT_PHRASES: &[&str] = &[
    "overloaded",
    "rate limit",
    "rate_limit",
    "timeout",
    "timed out",
    "capacity",
    "temporarily unavailable",
    "service unavailable",
    "connection reset",
    "econnreset",
    "etimedout",
];

/// HTTP status codes that mark a provider transient (5xx / 429 / Anthropic 529). Matched
/// STRUCTURALLY — never as a raw substring — because a bare number collides constantly: a plain
/// `contains("500")` fires on `AssertionError: expected 500 got 200` (a real code bug misread as
/// transient, so it is never diagnosed) and even on `processed 5000 rows` (the substring `500` sits
/// inside `5000`). A code counts only when it stands alone as a whole token AND the message is
/// HTTP-shaped: it carries an `http`/`status` context word, or the code is the leading token
/// (`502 Bad Gateway`). Otherwise the number is treated as ordinary prose.
const TRANSIENT_CODES: &[&str] = &["429", "500", "502", "503", "504", "529"];

pub(crate) fn classify(status: Option<&str>, error: Option<&str>) -> Class {
    if status == Some("timeout") {
        return Class::Transient;
    }
    let e = error.unwrap_or_default().to_lowercase();
    if TRANSIENT_PHRASES.iter().any(|m| e.contains(m)) {
        return Class::Transient;
    }
    // Tokenize on non-alphanumeric boundaries so a code is compared as a whole token, and require
    // HTTP context (an `http`/`status` word, or the code leading the message) before a bare number
    // is read as a status code.
    let tokens: Vec<&str> = e
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let http_context = tokens.iter().any(|t| *t == "http" || *t == "status");
    let code_hit = TRANSIENT_CODES
        .iter()
        .any(|code| tokens.contains(code) && (http_context || tokens.first() == Some(code)));
    if code_hit {
        return Class::Transient;
    }
    Class::Code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_errors_are_transient() {
        assert_eq!(
            classify(Some("error"), Some("HTTP 529 overloaded")),
            Class::Transient
        );
        assert_eq!(
            classify(Some("error"), Some("rate_limit_exceeded: retry")),
            Class::Transient
        );
        assert_eq!(classify(Some("timeout"), None), Class::Transient);
    }

    #[test]
    fn code_errors_are_investigated() {
        assert_eq!(
            classify(
                Some("error"),
                Some("TypeError: cannot read properties of undefined")
            ),
            Class::Code
        );
        assert_eq!(
            classify(Some("error"), Some("failed to parse model JSON response")),
            Class::Code
        );
    }

    #[test]
    fn a_bare_number_that_is_not_an_http_code_is_investigated() {
        // The exact false-transient the structural match closes: a code bug whose message merely
        // CONTAINS a status-shaped number must be diagnosed, not skipped as a provider hiccup.
        assert_eq!(
            classify(Some("error"), Some("AssertionError: expected 500 got 200")),
            Class::Code
        );
        // A magnitude that a substring match would see as `500` inside `5000`.
        assert_eq!(
            classify(Some("error"), Some("processed 5000 rows then panicked")),
            Class::Code
        );
        // But a genuine HTTP status is still transient — leading code and http-context both work.
        assert_eq!(
            classify(Some("error"), Some("502 Bad Gateway")),
            Class::Transient
        );
        assert_eq!(
            classify(Some("error"), Some("upstream returned HTTP 500")),
            Class::Transient
        );
    }
}
