use regex::RegexBuilder;

use lighttrack_core::RubricDimension;

use super::{snip, verdict};
use crate::{EngineError, Result};

pub(super) fn evaluate(
    dimension: &RubricDimension,
    target: &str,
    subject: &str,
    tolerance: f64,
) -> Result<(Option<f64>, String)> {
    let integer_target = integer_literal(target);
    let decimal_target = if integer_target {
        None
    } else {
        Some(target.parse::<f64>().map_err(|_| {
            EngineError::Other(format!(
                "rubric dimension '{}' (numeric) target `{target}` is not a number",
                dimension.key
            ))
        })?)
    };

    Ok(match first_number(subject) {
        None => (
            Some(0.0),
            format!(
                "numeric: expected `{target}`, no number in `{}` → fail",
                snip(subject)
            ),
        ),
        Some(actual) if integer_target => verdict(
            exact_numbers_equal(target, actual),
            format!(
                "numeric: expected `{target}`, got `{actual}`, integer target compares exactly"
            ),
        ),
        Some(actual) => {
            let expected = decimal_target.expect("non-integer target was parsed above");
            let Ok(actual_number) = actual.parse::<f64>() else {
                return Ok((
                    Some(0.0),
                    format!(
                        "numeric: expected `{target}`, no number in `{}` → fail",
                        snip(subject)
                    ),
                ));
            };
            verdict(
                (actual_number - expected).abs() <= tolerance,
                format!(
                    "numeric: expected `{expected}`, got `{actual_number}`, tolerance {tolerance}"
                ),
            )
        }
    })
}

fn integer_literal(s: &str) -> bool {
    let s = s.trim();
    let digits = s.strip_prefix(['-', '+']).unwrap_or(s);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Compare decimal spellings without converting through a fixed-width integer or float.
fn exact_numbers_equal(a: &str, b: &str) -> bool {
    canonical_number(a) == canonical_number(b)
}

/// `(negative, significant digits, base-10 exponent)` for exact numeric equality.
fn canonical_number(s: &str) -> Option<(bool, String, i64)> {
    let s = s.trim();
    let (negative, unsigned) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(i) => (&unsigned[..i], unsigned[i + 1..].parse::<i64>().ok()?),
        None => (unsigned, 0),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some(parts) => parts,
        None => (mantissa, ""),
    };
    if whole.is_empty() && fraction.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }

    let combined = mantissa.replace('.', "");
    let digits = combined.trim_start_matches('0');
    if digits.is_empty() {
        return Some((false, String::new(), 0));
    }
    let significant = digits.trim_end_matches('0');
    let trailing_zeroes = digits.len() - significant.len();
    let exponent = exponent
        .checked_sub(i64::try_from(fraction.len()).ok()?)?
        .checked_add(i64::try_from(trailing_zeroes).ok()?)?;
    Some((negative, significant.to_string(), exponent))
}

/// The output's number: the whole (trimmed) subject if it parses, else its first numeric token.
fn first_number(s: &str) -> Option<&str> {
    if s.trim().parse::<f64>().is_ok() {
        return Some(s.trim());
    }
    let re = RegexBuilder::new(r"[-+]?\d+(?:\.\d+)?(?:[eE][-+]?\d+)?")
        .build()
        .ok()?;
    re.find(s).map(|m| m.as_str())
}
