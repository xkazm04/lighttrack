//! Cheap triage: decide whether a failure is a transient/provider problem (no code change can fix it,
//! so don't spend an investigation) or a code-side issue worth pointing Claude Code at.

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// Provider/infra hiccup — rate limit, overload, 5xx, timeout. Skip.
    Transient,
    /// Contract/integration or application-logic failure. Investigate.
    Code,
}

/// Substrings that mark a provider- or network-side transient error. Matched case-insensitively
/// against the error message; the `timeout` status is transient on its own.
const TRANSIENT_MARKERS: &[&str] = &[
    "429", "overloaded", "rate limit", "rate_limit", "timeout", "timed out", "capacity",
    "temporarily unavailable", "service unavailable", "connection reset", "econnreset", "etimedout",
    "500", "502", "503", "504", "529",
];

pub(crate) fn classify(status: Option<&str>, error: Option<&str>) -> Class {
    if status == Some("timeout") {
        return Class::Transient;
    }
    let e = error.unwrap_or_default().to_lowercase();
    if TRANSIENT_MARKERS.iter().any(|m| e.contains(m)) {
        return Class::Transient;
    }
    Class::Code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_errors_are_transient() {
        assert_eq!(classify(Some("error"), Some("HTTP 529 overloaded")), Class::Transient);
        assert_eq!(classify(Some("error"), Some("rate_limit_exceeded: retry")), Class::Transient);
        assert_eq!(classify(Some("timeout"), None), Class::Transient);
    }

    #[test]
    fn code_errors_are_investigated() {
        assert_eq!(
            classify(Some("error"), Some("TypeError: cannot read properties of undefined")),
            Class::Code
        );
        assert_eq!(classify(Some("error"), Some("failed to parse model JSON response")), Class::Code);
    }
}
