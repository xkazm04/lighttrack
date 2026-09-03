//! Cheap triage: decide whether a failure is a transient/provider problem (no code change can fix it,
//! so don't spend an investigation) or a code-side issue worth pointing Claude Code at.
//!
//! **The class is carried, not re-derived.** `crates/engine/src/retry.rs` states the rule the
//! workspace follows — *"Classification is by typed `EngineError` variant — never by string-matching
//! provider messages"* — and this crate could not follow it, because it classifies an error it did
//! not produce, arriving as an `Option<String>` that crossed a process boundary. The fix was not to
//! delete the phrase list; it was to mint the class where the structured response still exists (the
//! SDK, in the caller's process), carry it on the event (`metadata.failure_class`), and put it in
//! the alert payload. [`decide`] reads that first.
//!
//! **The phrase list stays, and its scope is now stateable.** It is the correct handling for a
//! record whose producer said nothing: an older SDK, a third-party producer, an OTLP export. It is
//! no longer the primary path, and every use of it is counted ([`Source::Fallback`]) so "how often
//! are we still guessing" is a number rather than an impression. A text classifier is not
//! forbidden — it is forbidden where structure was available and discarded.

use lighttrack_core::FailureClass;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Class {
    /// Provider/infra hiccup — rate limit, overload, 5xx, timeout. Skip.
    Transient,
    /// Contract/integration or application-logic failure. Investigate.
    Code,
}

/// Where a verdict came from. Not cosmetic: it is the instrument for the only question that decides
/// whether carrying the class was worth doing — how many decisions are still being made by reading
/// prose.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Source {
    /// The producer said so, at the boundary, from the structured response.
    Carried,
    /// The producer said nothing (or is too old to say anything) and the message was read.
    Fallback,
}

/// The decision, and how it was reached.
///
/// Three carried states map to three different code paths, which is what earns `Unknown` its place:
/// `Terminal` goes straight to an investigation **without** consulting the message, so a genuine
/// bug whose text happens to read "connection reset" is still diagnosed; `Unknown` is the only one
/// that runs [`classify`].
pub(crate) fn decide(
    carried: Option<&str>,
    status: Option<&str>,
    error: Option<&str>,
) -> (Class, Source) {
    match carried.map(FailureClass::from_wire) {
        Some(FailureClass::Transient) => (Class::Transient, Source::Carried),
        Some(FailureClass::Terminal) => (Class::Code, Source::Carried),
        // Absent, `unknown`, or a value outside the vocabulary: nobody with the structured response
        // told us, so read the message — and say that is what happened.
        Some(FailureClass::Unknown) | None => (classify(status, error), Source::Fallback),
    }
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

/// The fallback: read the message. Only reachable from [`decide`] when the producer said nothing.
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

    // --- the measurable: classifier disagreement -------------------------------------------------
    //
    // The falsifier this direction rests on, and the cheapest one available: replay a window of
    // failures through BOTH paths and count the rows where they differ. If the phrase list already
    // reached the verdict the producer would have, then the schema field, the SDK changes and the
    // ingest work are waste and the honest change is a comment at retry.rs:3-4. So the number is
    // measured here rather than asserted.
    //
    // Each row is (the producer's TRUE class — what an SDK still holding the provider's response
    // would have minted, the status, the message as it actually crosses the wire). The shapes are
    // real: the Anthropic and OpenAI Python SDKs, litellm, httpx, gRPC and psycopg, plus the
    // application errors an app emits around them.

    const CORPUS: &[(FailureClass, Option<&str>, &str)] = &[
        // Provider transients the phrase list catches.
        (
            FailureClass::Transient,
            Some("error"),
            "overloaded_error: Overloaded",
        ),
        (
            FailureClass::Transient,
            Some("error"),
            "rate_limit_error: number of request tokens has exceeded your per-minute rate limit",
        ),
        (
            FailureClass::Transient,
            Some("error"),
            "429 Too Many Requests",
        ),
        (
            FailureClass::Transient,
            Some("error"),
            "upstream returned HTTP 503",
        ),
        (
            FailureClass::Transient,
            Some("timeout"),
            "httpx.ReadTimeout",
        ),
        (
            FailureClass::Transient,
            Some("error"),
            "litellm.Timeout: Connection timed out after 600s",
        ),
        (FailureClass::Transient, Some("error"), "502 Bad Gateway"),
        // Provider transients the phrase list MISSES. Each of these is an investigation the
        // responder pays for against a codebase with no bug in it.
        (
            FailureClass::Transient,
            Some("error"),
            "anthropic.InternalServerError: Internal server error",
        ),
        (
            FailureClass::Transient,
            Some("error"),
            "openai.APIConnectionError: Connection error.",
        ),
        (
            FailureClass::Transient,
            Some("error"),
            "upstream connect error or disconnect/reset before headers",
        ),
        (
            FailureClass::Transient,
            Some("error"),
            "gRPC error: DEADLINE_EXCEEDED",
        ),
        (
            FailureClass::Transient,
            Some("error"),
            "psycopg.OperationalError: server closed the connection unexpectedly",
        ),
        // Terminal failures the phrase list gets right.
        (
            FailureClass::Terminal,
            Some("error"),
            "TypeError: cannot read properties of undefined (reading choices)",
        ),
        (
            FailureClass::Terminal,
            Some("error"),
            "AuthenticationError: invalid x-api-key",
        ),
        (
            FailureClass::Terminal,
            Some("error"),
            "NotFoundError: model gpt-5-turbo does not exist",
        ),
        (
            FailureClass::Terminal,
            Some("error"),
            "BadRequestError: max_tokens is too large: 200000",
        ),
        (
            FailureClass::Terminal,
            Some("error"),
            "failed to parse model JSON response",
        ),
        // Terminal failures the phrase list calls transient — a real bug skipped, and therefore
        // never diagnosed. The worst direction, and the one the numeric half of this file was
        // already hardened against for status codes.
        (
            FailureClass::Terminal,
            Some("error"),
            "AssertionError: timed out waiting for the retry mock to be called",
        ),
        (
            FailureClass::Terminal,
            Some("error"),
            "ValueError: the rate limit column is missing from the response schema",
        ),
        (
            FailureClass::Terminal,
            Some("error"),
            "KeyError: capacity is not present in the plan config",
        ),
    ];

    fn expected(c: FailureClass) -> Class {
        match c {
            FailureClass::Transient => Class::Transient,
            _ => Class::Code,
        }
    }

    #[test]
    fn the_two_paths_disagree_and_the_carried_one_is_the_right_one() {
        let mut prose_wrong = Vec::new();
        let mut carried_wrong = Vec::new();
        for (truth, status, msg) in CORPUS {
            if classify(*status, Some(msg)) != expected(*truth) {
                prose_wrong.push(*msg);
            }
            let (class, source) = decide(Some(truth.as_str()), *status, Some(msg));
            assert_eq!(
                source,
                Source::Carried,
                "a stated class must not be re-derived: {msg}"
            );
            if class != expected(*truth) {
                carried_wrong.push(*msg);
            }
        }
        // BEFORE: every decision came from the message, and this many of them were wrong.
        assert_eq!(
            prose_wrong.len(),
            DISAGREEMENTS,
            "the size of the defect, over {} rows. If this number moved, the phrase list changed \
             and that change is what needs justifying:\n{}",
            CORPUS.len(),
            prose_wrong.join("\n")
        );
        // AFTER: the producer's own verdict, carried, is right by construction.
        assert!(carried_wrong.is_empty(), "{carried_wrong:?}");
    }

    /// Measured, not chosen: the rows of [`CORPUS`] where reading the message reaches a different
    /// verdict than the producer would have. Pinned so a future edit to the phrase list has to move
    /// a number a reviewer can see.
    const DISAGREEMENTS: usize = 8;

    #[test]
    fn a_terminal_message_that_reads_transient_is_still_investigated() {
        // The single most valuable row in the corpus: a real bug whose text contains "timed out".
        // The prose classifier skips it, so it is never diagnosed; the carried class does not.
        let msg = "AssertionError: timed out waiting for the retry mock to be called";
        assert_eq!(classify(Some("error"), Some(msg)), Class::Transient);
        assert_eq!(
            decide(Some("terminal"), Some("error"), Some(msg)),
            (Class::Code, Source::Carried)
        );
    }

    #[test]
    fn unknown_is_not_terminal_it_is_the_only_state_that_reads_the_message() {
        // The third state earns its place only if a consumer treats it differently from the other
        // two. It does: this is the ONE input that reaches `classify` at all.
        let msg = "overloaded_error: Overloaded";
        assert_eq!(
            decide(Some("unknown"), Some("error"), Some(msg)),
            (Class::Transient, Source::Fallback)
        );
        assert_eq!(
            decide(None, Some("error"), Some(msg)),
            (Class::Transient, Source::Fallback)
        );
        // Terminal does NOT consult the message, so a provider-shaped phrase cannot overturn it.
        assert_eq!(
            decide(Some("terminal"), Some("error"), Some(msg)),
            (Class::Code, Source::Carried)
        );
    }

    #[test]
    fn a_class_outside_the_vocabulary_is_quarantined_not_trusted() {
        // A producer that invents a word gets the fallback, never a silent coercion into a real
        // class that a downstream match would then act on.
        assert_eq!(
            decide(Some("retryable"), Some("error"), Some("TypeError: nope")),
            (Class::Code, Source::Fallback)
        );
    }

    #[test]
    fn the_counter_shows_how_many_decisions_are_still_guesses() {
        use crate::pipeline::Classified;
        // BEFORE: nothing carried a class, so every decision was read from prose.
        let before = Classified::default();
        for (_, status, msg) in CORPUS {
            let (class, source) = decide(None, *status, Some(msg));
            before.record(class, source);
        }
        assert_eq!(before.fallback_rate(), 1.0);

        // AFTER: a producer that mints the class is decided from it.
        let after = Classified::default();
        for (truth, status, msg) in CORPUS {
            let (class, source) = decide(Some(truth.as_str()), *status, Some(msg));
            after.record(class, source);
        }
        assert_eq!(after.fallback_rate(), 0.0);
        assert_eq!(
            after.get(Class::Transient, Source::Carried) + after.get(Class::Code, Source::Carried),
            CORPUS.len() as u64
        );
    }
}
