//! The effort ladder, printed: what each rung bought over the rung below it.
//!
//! Like every other block in this crate it **derives nothing** — the runner computes the medians,
//! the flip counts and the sentence, and this layer only lays them out. A summary without the key
//! prints nothing at all, so a matrix with no effort ladder renders exactly as it always did.

use serde_json::Value;

use crate::md::{opt_f, s, u};

/// One step's per-tier lines, indented under it. Only the tiers the runner sent, in its order.
fn tier_lines(step: &Value) -> String {
    let Some(tiers) = step.get("tiers").and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for t in tiers {
        let thinking = match (
            opt_f(t, "median_thinking_from"),
            opt_f(t, "median_thinking_to"),
        ) {
            (Some(a), Some(b)) => format!(", median thinking {a:.0}→{b:.0}"),
            _ => String::new(),
        };
        out.push_str(&format!(
            "  - _{}_ ({} case(s)): score {:+.3}, {} improved / {} regressed{thinking}.\n",
            s(t, "tier"),
            u(t, "n_cases"),
            opt_f(t, "mean_score_delta").unwrap_or_default(),
            u(t, "improved"),
            u(t, "regressed"),
        ));
    }
    out
}

pub(crate) fn block(v: Option<&Value>) -> Option<String> {
    let curve = v.filter(|c| c.is_object())?;
    let models = curve
        .get("models")
        .and_then(Value::as_array)
        .filter(|m| !m.is_empty())?;
    let mut out = format!("\n_Effort curve — {}:_\n\n", s(curve, "note"));
    for m in models {
        for step in m.get("steps").and_then(Value::as_array).unwrap_or(&vec![]) {
            out.push_str(&format!(
                "- **{} {}→{}** ({} shared case(s)): {}.\n",
                s(m, "model"),
                s(step, "from_effort"),
                s(step, "to_effort"),
                u(step, "n_cases"),
                s(step, "verdict"),
            ));
            out.push_str(&tier_lines(step));
        }
    }
    for c in curve
        .get("caveats")
        .and_then(Value::as_array)
        .unwrap_or(&vec![])
        .iter()
        .filter_map(Value::as_str)
    {
        out.push_str(&format!("\n_Caveat: {c}._\n"));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn curve() -> Value {
        json!({
            "note": "descriptive, within one model",
            "caveats": ["one draw per case: a flipped case may be sampling noise"],
            "models": [{
                "model": "anthropic/sonnet",
                "steps": [{
                    "from": "sonnet@low", "to": "sonnet@high",
                    "from_effort": "low", "to_effort": "high",
                    "n_cases": 18, "basis": "output_tokens", "dial": "alive",
                    "improved": 1, "regressed": 0, "unchanged": 17,
                    "mean_score_delta": 0.028,
                    "verdict": "thinking ×9.6 (median 360→3472 output tokens); 1 improved, 0 regressed, 17 unchanged; mean score +0.028 — the extra thinking paid, on the cases that flipped",
                    "tiers": [
                        { "tier": "easy", "n_cases": 9, "mean_score_delta": 0.0, "improved": 0, "regressed": 0,
                          "median_thinking_from": 300.0, "median_thinking_to": 900.0 },
                        { "tier": "hard", "n_cases": 9, "mean_score_delta": 0.056, "improved": 1, "regressed": 0,
                          "median_thinking_from": 420.0, "median_thinking_to": 6000.0 }
                    ]
                }]
            }]
        })
    }

    /// The sentence and its numbers reach the reader verbatim, with the per-tier breakdown under it.
    #[test]
    fn the_step_verdict_and_its_tiers_are_printed_as_handed_over() {
        let md = block(Some(&curve())).expect("a curve renders");
        assert!(
            md.contains("**anthropic/sonnet low→high** (18 shared case(s)): thinking ×9.6"),
            "{md}"
        );
        assert!(md.contains("median 360→3472 output tokens"), "{md}");
        assert!(
            md.contains("_easy_ (9 case(s)): score +0.000, 0 improved / 0 regressed, median thinking 300→900."),
            "the tier that bought nothing is legible: {md}"
        );
        assert!(md.contains("_hard_ (9 case(s)): score +0.056"), "{md}");
        assert!(md.contains("Caveat: one draw per case"), "{md}");
    }

    /// Nothing handed over → no block, exactly like every other optional block here.
    #[test]
    fn an_absent_or_empty_curve_prints_nothing() {
        assert!(block(None).is_none());
        assert!(block(Some(&json!({ "note": "x", "models": [] }))).is_none());
        assert!(block(Some(&json!("not an object"))).is_none());
    }

    /// A step with no tier breakdown (an ungraded corpus) prints its line and no sub-lines.
    #[test]
    fn an_ungraded_step_prints_only_its_verdict_line() {
        let mut c = curve();
        c["models"][0]["steps"][0]
            .as_object_mut()
            .unwrap()
            .remove("tiers");
        let md = block(Some(&c)).unwrap();
        assert!(md.contains("low→high"));
        assert!(!md.contains("  - _"), "no empty tier list: {md}");
    }
}
