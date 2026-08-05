//! Tests for the PII scrubber.
//!
//! Two happy-path examples plus a generative **property suite**: for each structured-PII shape we
//! generate many randomized-but-valid instances and assert the compliance invariant the
//! redact-on-ingest feature leans on — after [`scrub`], re-running *every* rule regex over the
//! output finds zero matches (no raw PII shape survives). The mixed-corpus test exercises the
//! rule-ordering claim at scale (an IP must not be eaten by the phone/CC rules), and the guards
//! pin down false positives (a `1.2.3` version must not be mangled into `<CC>`).
//!
//! The false-positive guards carry equal weight to the no-leak invariant here, because they are the
//! only thing watching that side: a leaked address is discoverable in the stored row, whereas text
//! the scrubber rewrote reads as ordinary prose to every consumer downstream, the judge included.
//! The strings in [`iso_dates_and_versions_are_not_phone_numbers`] and
//! [`card_redaction_preserves_surrounding_whitespace`] are the observed corruptions, verbatim.

use super::*;

// ---------------------------------------------------------------------------
// Happy-path examples (the original two, kept as regression anchors)
// ---------------------------------------------------------------------------

#[test]
fn scrubs_common_pii() {
    let s = scrub(
        "Contact john.doe@example.com or call +1 (415) 555-2671. \
         Card 4111 1111 1111 1111, server 10.0.0.1, key sk-abcd1234efgh5678ijkl.",
    );
    assert!(s.text.contains("<EMAIL>"), "{}", s.text);
    assert!(s.text.contains("<PHONE>"), "{}", s.text);
    assert!(s.text.contains("<CC>"), "{}", s.text);
    assert!(s.text.contains("<IP>"), "{}", s.text);
    assert!(s.text.contains("<SECRET>"), "{}", s.text);
    assert!(!s.text.contains("john.doe@example.com"));
    assert!(!s.text.contains("4111"));
    assert!(s.redactions >= 5, "redactions={}", s.redactions);
}

#[test]
fn leaves_clean_text_untouched() {
    let s = scrub("The capital of France is Paris.");
    assert_eq!(s.text, "The capital of France is Paris.");
    assert_eq!(s.redactions, 0);
}

// ---------------------------------------------------------------------------
// Deterministic PRNG + PII shape generators (no external rng dependency, so a
// failure always reproduces from its fixed seed)
// ---------------------------------------------------------------------------

/// xorshift64* — small, fast, deterministic.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // `| 1` guarantees a non-zero state (xorshift is stuck at zero).
        Rng((seed ^ 0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in `[0, n)`.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    /// Uniform in `[lo, hi]` inclusive.
    fn between(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
    /// True roughly one time in `one_in`.
    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }
}

const ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const LOWER: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
const UPPER: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGITS: &[u8] = b"0123456789";
const HEX: &[u8] = b"0123456789abcdefABCDEF";
const UPPER_DIGIT: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
// `sk-` secret body: matches the rule's `[A-Za-z0-9_\-]` character class.
const SK_BODY: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-";
// email local part: matches the rule's `[A-Za-z0-9._%+\-]` character class.
const EMAIL_LOCAL: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._%-";

fn push_chars(rng: &mut Rng, out: &mut String, charset: &[u8], n: usize) {
    for _ in 0..n {
        out.push(*rng.pick(charset) as char);
    }
}

/// Like [`push_chars`] but with a random length in `[lo, hi]` — computed first so `rng` isn't
/// borrowed twice in one argument list.
fn push_rand(rng: &mut Rng, out: &mut String, charset: &[u8], lo: usize, hi: usize) {
    let n = rng.between(lo, hi);
    push_chars(rng, out, charset, n);
}

fn gen_email(rng: &mut Rng) -> String {
    let mut s = String::new();
    s.push(*rng.pick(ALNUM) as char); // start alnum
    push_rand(rng, &mut s, EMAIL_LOCAL, 0, 8);
    if rng.chance(2) {
        // a "+tag" subaddress
        s.push('+');
        push_rand(rng, &mut s, ALNUM, 1, 6);
    }
    s.push('@');
    push_rand(rng, &mut s, LOWER, 1, 8);
    if rng.chance(2) {
        s.push('.'); // a second domain label
        push_rand(rng, &mut s, LOWER, 1, 6);
    }
    s.push('.');
    push_rand(rng, &mut s, LOWER, 2, 6); // TLD ≥ 2 letters
    s
}

fn gen_ssn(rng: &mut Rng) -> String {
    format!(
        "{:03}-{:02}-{:04}",
        rng.below(1000),
        rng.below(100),
        rng.below(10000)
    )
}

fn gen_iban(rng: &mut Rng) -> String {
    let mut s = String::new();
    push_chars(rng, &mut s, UPPER, 2); // country
    push_chars(rng, &mut s, DIGITS, 2); // check digits
    push_rand(rng, &mut s, UPPER_DIGIT, 10, 30); // BBAN
    s
}

fn gen_secret(rng: &mut Rng) -> String {
    match rng.below(3) {
        0 => {
            let mut s = String::from("sk-");
            let n = rng.between(16, 40);
            push_chars(rng, &mut s, SK_BODY, n - 1);
            s.push(*rng.pick(ALNUM) as char); // end on a word char for the trailing \b
            s
        }
        1 => {
            let mut s = String::from("AKIA");
            push_rand(rng, &mut s, UPPER_DIGIT, 12, 24);
            s
        }
        _ => {
            let mut s = String::new();
            push_rand(rng, &mut s, HEX, 32, 64); // 32+ hex digit "token"
            s
        }
    }
}

fn gen_card(rng: &mut Rng) -> String {
    let n = *rng.pick(&[15usize, 16, 19]);
    let sep = *rng.pick(&["", " ", "-"]);
    let mut s = String::new();
    for i in 0..n {
        if i > 0 && i % 4 == 0 && !sep.is_empty() {
            s.push_str(sep);
        }
        s.push(*rng.pick(DIGITS) as char);
    }
    s
}

fn gen_ip(rng: &mut Rng) -> String {
    format!(
        "{}.{}.{}.{}",
        rng.below(256),
        rng.below(256),
        rng.below(256),
        rng.below(256)
    )
}

/// Only the four shapes the phone rules recognize. The rules are deliberately conservative — a
/// bare digit run with no `+`, no parens and no 3-3-4 grouping is indistinguishable from a date or
/// a version, so it is left alone — and this generator states that contract.
fn gen_phone(rng: &mut Rng) -> String {
    match rng.below(4) {
        // "+420 123 456 789" — country code plus separated groups.
        0 => {
            let mut s = String::from("+");
            push_rand(rng, &mut s, DIGITS, 1, 3);
            for _ in 0..rng.between(2, 5) {
                s.push(*rng.pick(&[' ', '-']));
                push_rand(rng, &mut s, DIGITS, 2, 4);
            }
            s
        }
        // "+14155552671" / "+420 123456789" — E.164 with at most one separator.
        1 => {
            let mut s = String::from("+");
            push_rand(rng, &mut s, DIGITS, 1, 3);
            if rng.chance(2) {
                s.push(*rng.pick(&[' ', '-']));
            }
            push_rand(rng, &mut s, DIGITS, 7, 12);
            s
        }
        // "(415) 555-2671" — parenthesized area code.
        2 => {
            let mut s = String::from("(");
            push_rand(rng, &mut s, DIGITS, 2, 4);
            s.push(')');
            for i in 0..rng.between(2, 4) {
                if i > 0 || rng.chance(2) {
                    s.push(*rng.pick(&[' ', '-']));
                }
                push_rand(rng, &mut s, DIGITS, 2, 4);
            }
            s
        }
        // "415-555-2671" — NANP 3-3-4.
        _ => {
            let sep = *rng.pick(&[' ', '-', '.']);
            let mut s = String::new();
            push_chars(rng, &mut s, DIGITS, 3);
            s.push(sep);
            push_chars(rng, &mut s, DIGITS, 3);
            s.push(sep);
            push_chars(rng, &mut s, DIGITS, 4);
            s
        }
    }
}

/// A named PII-shape generator: `(kind, fn)`.
type Generator = (&'static str, fn(&mut Rng) -> String);

const GENERATORS: &[Generator] = &[
    ("email", gen_email),
    ("ssn", gen_ssn),
    ("iban", gen_iban),
    ("secret", gen_secret),
    ("card", gen_card),
    ("ip", gen_ip),
    ("phone", gen_phone),
];

// ---------------------------------------------------------------------------
// The core invariant
// ---------------------------------------------------------------------------

/// Re-run *every* rule's regex over scrubbed text and assert none matches. A surviving match is a
/// raw-PII leak past the redaction boundary.
fn assert_no_pii_survives(input: &str) {
    let out = scrub(input).text;
    for rule in rules() {
        assert!(
            !rule.re.is_match(&out),
            "PII shape {} survived scrub\n  input:  {input:?}\n  output: {out:?}",
            rule.placeholder,
        );
    }
}

#[test]
fn property_no_pii_shape_survives_scrub() {
    let mut rng = Rng::new(0x00C0_FFEE);
    for round in 0..3000usize {
        let (kind, gen) = GENERATORS[round % GENERATORS.len()];
        let token = gen(&mut rng);
        // Teeth: every generated token is genuine PII, so it must redact — this keeps the no-leak
        // assertion from passing vacuously on a generator that emitted an unmatchable string.
        let bare = scrub(&token);
        assert!(
            bare.redactions >= 1,
            "{kind} generator emitted no redactable PII: {token:?}"
        );
        assert_no_pii_survives(&token); // bare
        assert_no_pii_survives(&format!("please contact {token} as soon as possible")); // in prose
        assert_no_pii_survives(&format!("ref: [{token}].")); // hugged by punctuation
    }
}

#[test]
fn property_mixed_corpus_is_idempotent() {
    let mut rng = Rng::new(0x1234_5678);
    for _ in 0..600 {
        let k = rng.between(2, 6);
        let parts: Vec<String> = (0..k)
            .map(|_| {
                let gen = rng.pick(GENERATORS).1;
                gen(&mut rng)
            })
            .collect();
        let sentence = format!("log: {} done", parts.join(" | "));

        let first = scrub(&sentence);
        for rule in rules() {
            assert!(
                !rule.re.is_match(&first.text),
                "leak {}: {sentence:?} -> {:?}",
                rule.placeholder,
                first.text,
            );
        }
        // Idempotence is the strongest no-leak statement: a second pass redacts nothing more.
        let second = scrub(&first.text);
        assert_eq!(
            second.redactions, 0,
            "second pass: {:?} -> {:?}",
            first.text, second.text
        );
        assert_eq!(second.text, first.text);
    }
}

// ---------------------------------------------------------------------------
// Rule-ordering + placeholder typing (the doc claim, made explicit)
// ---------------------------------------------------------------------------

#[test]
fn ip_is_not_eaten_by_phone_or_cc() {
    // Rule order (IP before PHONE; dots break the CC class) must keep an IP typed as <IP> —
    // note 192.168.100.200 *would* match the greedy phone rule if it ran first.
    for ip in ["10.0.0.1", "192.168.100.200", "8.8.8.8", "255.255.255.255"] {
        let s = scrub(ip);
        assert_eq!(s.text, "<IP>", "ip {ip:?} -> {:?}", s.text);
        assert_eq!(s.redactions, 1);
    }
}

#[test]
fn structured_pii_maps_to_expected_placeholder() {
    let cases = [
        ("john.doe+filter@sub.example.co.uk", "<EMAIL>"),
        ("123-45-6789", "<SSN>"),
        ("DE89370400440532013000", "<IBAN>"),
        ("sk-abcd1234efgh5678ijkl", "<SECRET>"),
        ("AKIAIOSFODNN7EXAMPLE", "<SECRET>"),
        ("0123456789abcdef0123456789abcdef", "<SECRET>"), // 32 hex digits
        ("4111 1111 1111 1111", "<CC>"),
    ];
    for (raw, placeholder) in cases {
        assert_eq!(scrub(raw).text, placeholder, "{raw:?}");
    }
}

// ---------------------------------------------------------------------------
// False-positive guards: benign text must pass through untouched
// ---------------------------------------------------------------------------

#[test]
fn benign_text_is_not_over_redacted() {
    let clean = [
        "The capital of France is Paris.",
        "version 1.2.3 shipped", // not a <CC> / <IP>
        "upgrade to v2.10.4 today",
        "see RFC 3339 for timestamps",
        "ticket #4827 was resolved",
        "the answer is 42",
        "commit a1b2c3d landed", // short hex, below the 32-char secret threshold
        "let total_count_of_all_active_users = 0", // long identifier, not a hex secret
        "pi is roughly 3.14159",
        "the run finished +100000 rows ahead", // a signed count, not an E.164 number
        "999.999.999.999 is not an address",   // impossible quad, so not an <IP> either
    ];
    for text in clean {
        let s = scrub(text);
        assert_eq!(s.text, text, "over-redacted {text:?} -> {:?}", s.text);
        assert_eq!(s.redactions, 0, "{text:?}");
    }
}

/// The one ambiguity the shapes cannot resolve, pinned so it is a known cost rather than a
/// surprise: a four-component version string *is* a well-formed dotted quad, and no regex can tell
/// `4.10.2.1` the release from `4.10.2.1` the host. It types as `<IP>`. Three-component versions —
/// overwhelmingly the common case, and the one the old rules also mangled — are safe.
#[test]
fn four_component_versions_are_indistinguishable_from_addresses() {
    assert_eq!(scrub("build 4.10.2.1 shipped").text, "build <IP> shipped");
    assert_eq!(scrub("build 4.10.2 shipped").redactions, 0);
}

/// Regression, verbatim. The old phone rule `\+?\d[\d\s().\-]{8,}\d` matched *any* run of ten-odd
/// digit-ish characters, so every ISO date and dotted version in captured text became `<PHONE>`.
/// This is the defect no downstream signal can surface: a judge scored the mangled sentence 0.88
/// and the clean one 0.85 without ever remarking on the vanished date.
#[test]
fn iso_dates_and_versions_are_not_phone_numbers() {
    for text in [
        "2026-07-01",
        "2024-11-30",
        "1.2.3-4.5.6",
        "What is the return window for an item bought on 2026-07-01, and do I pay return shipping?",
        "the window runs 2026-07-01 to 2026-07-08",
        "ISO 8601 timestamp 2026-07-01T12:00:00Z",
        "e2e run 2026-07-01 12:34:56",
        "upgraded from 1.2.3-4.5.6 to 1.2.4-4.5.6",
    ] {
        let s = scrub(text);
        assert_eq!(s.text, text, "mangled {text:?} -> {:?}", s.text);
        assert_eq!(s.redactions, 0, "{text:?}");
    }
}

/// Regression, verbatim. The card rule's last repetition (`\d[ \-]?`) consumed its trailing
/// separator, welding the placeholder to the next word: `card <CC>was`.
#[test]
fn card_redaction_preserves_surrounding_whitespace() {
    let s = scrub("card 4111 1111 1111 1111 was");
    assert_eq!(s.text, "card <CC> was");
    assert_eq!(s.redactions, 1);

    // Hyphen grouping and a trailing hyphen-as-punctuation must survive too.
    assert_eq!(scrub("card 4111-1111-1111-1111 was").text, "card <CC> was");
    assert_eq!(scrub("pay 4111111111111111 now").text, "pay <CC> now");
}

#[test]
fn real_phone_formats_are_still_redacted() {
    for phone in [
        "+420 123 456 789",  // E.164, grouped
        "+420123456789",     // E.164, solid
        "+420 123456789",    // E.164, one separator
        "+1 415 555 2671",   // US with country code
        "+1 (415) 555-2671", // US with country code and area parens
        "+44 20 7946 0958",  // UK
        "(415) 555-2671",    // US, parenthesized area code
        "415-555-2671",      // NANP
        "415 555 2671",
        "415.555.2671",
    ] {
        let s = scrub(phone);
        assert_eq!(s.text, "<PHONE>", "phone {phone:?} -> {:?}", s.text);
        assert_eq!(s.redactions, 1, "{phone:?}");
        // …and in prose, with the surrounding words intact.
        let in_prose = scrub(&format!("call {phone} tomorrow"));
        assert_eq!(in_prose.text, "call <PHONE> tomorrow", "{phone:?}");
    }
}

#[test]
fn other_pii_shapes_are_still_redacted_in_prose() {
    let cases = [
        (
            "mail bob.smith+tag@example.co.uk today",
            "mail <EMAIL> today",
        ),
        ("ssn 123-45-6789 on file", "ssn <SSN> on file"),
        ("iban DE89370400440532013000 please", "iban <IBAN> please"),
        (
            "key sk-abcd1234efgh5678ijkl rotated",
            "key <SECRET> rotated",
        ),
        ("role AKIAIOSFODNN7EXAMPLE removed", "role <SECRET> removed"),
        (
            "token 0123456789abcdef0123456789abcdef expired",
            "token <SECRET> expired",
        ),
        ("host 192.168.100.200 down", "host <IP> down"),
    ];
    for (raw, expected) in cases {
        assert_eq!(scrub(raw).text, expected, "{raw:?}");
    }
}
