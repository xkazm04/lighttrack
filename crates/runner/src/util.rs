//! Small shared helpers: percentiles, dimension means, token-priced cost, claude resolution.

use std::collections::HashMap;

use serde_json::Value;

use lighttrack_core::ModelPriceRow;

/// p50/p95 of a latency sample (nearest-rank). Returns (None, None) if empty.
pub(crate) fn percentiles(latencies: &mut [u64]) -> (Option<u64>, Option<u64>) {
    if latencies.is_empty() {
        return (None, None);
    }
    latencies.sort_unstable();
    let pick = |p: f64| {
        let idx = (((latencies.len() - 1) as f64) * p).round() as usize;
        latencies[idx.min(latencies.len() - 1)]
    };
    (Some(pick(0.50)), Some(pick(0.95)))
}

/// Mean score of a dimension across `n` cases.
pub(crate) fn dim_mean(sums: &HashMap<String, f64>, key: &str, n: u32) -> f64 {
    sums.get(key).copied().unwrap_or(0.0) / n.max(1) as f64
}

/// A benchmark run's status against its recorded baseline: `regressed` if the mean dropped below it
/// (with a 1e-9 slack so float noise alone never trips a regression), `passed` if a baseline exists
/// and held, else `no_baseline`. Shared by rubric and simple modes so they verdict identically.
pub(crate) fn run_status(baseline: Option<f64>, mean: f64) -> &'static str {
    match baseline {
        Some(b) if mean + 1e-9 < b => "regressed",
        Some(_) => "passed",
        None => "no_baseline",
    }
}

/// Cost of a call from the DB price book (used when the provider API returns no $ cost).
pub(crate) fn price_gen_cost(
    prices: &[ModelPriceRow],
    provider: &str,
    model: &str,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
) -> f64 {
    prices
        .iter()
        .find(|p| p.provider == provider && p.model == model)
        .map(|p| {
            (input_tokens.unwrap_or(0) as f64) * p.input_per_mtok / 1_000_000.0
                + (output_tokens.unwrap_or(0) as f64) * p.output_per_mtok / 1_000_000.0
        })
        .unwrap_or(0.0)
}

/// Render a JSON value as plain text (strings as-is; everything else compact JSON).
pub(crate) fn value_to_text(v: &Value) -> String {
    match v.as_str() {
        Some(s) => s.to_string(),
        None => v.to_string(),
    }
}

/// First 8 chars of an id, for compact logging.
pub(crate) fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// Build a `ModelPriceRow` via serde so the test doesn't depend on chrono for `effective_date`.
    fn price(provider: &str, model: &str, input_per_mtok: f64, output_per_mtok: f64) -> ModelPriceRow {
        serde_json::from_value(json!({
            "provider": provider, "model": model,
            "input_per_mtok": input_per_mtok, "output_per_mtok": output_per_mtok,
        }))
        .unwrap()
    }

    #[test]
    fn percentiles_empty_is_none() {
        assert_eq!(percentiles(&mut []), (None, None));
    }

    #[test]
    fn percentiles_single_value() {
        assert_eq!(percentiles(&mut [42]), (Some(42), Some(42)));
    }

    #[test]
    fn percentiles_nearest_rank_and_sorts_in_place() {
        // 1..=10 shuffled; p50 → index round(9*0.5)=5 → value 6; p95 → index round(9*0.95)=9 → 10.
        let mut xs = [10, 1, 9, 2, 8, 3, 7, 4, 6, 5];
        assert_eq!(percentiles(&mut xs), (Some(6), Some(10)));
        assert_eq!(xs, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]); // sorted in place
    }

    #[test]
    fn dim_mean_divides_by_n_and_guards_zero() {
        let mut sums = HashMap::new();
        sums.insert("clarity".to_string(), 3.0);
        assert!(approx(dim_mean(&sums, "clarity", 4), 0.75));
        // Missing key → 0.0; n=0 is treated as 1 so we never divide by zero.
        assert!(approx(dim_mean(&sums, "missing", 4), 0.0));
        assert!(approx(dim_mean(&sums, "clarity", 0), 3.0));
    }

    #[test]
    fn price_gen_cost_from_book() {
        let prices = vec![price("openai", "gpt-4o", 2.5, 10.0)];
        // 1M input @2.5 + 0.5M output @10.0 = 2.5 + 5.0 = 7.5
        let c = price_gen_cost(&prices, "openai", "gpt-4o", Some(1_000_000), Some(500_000));
        assert!(approx(c, 7.5), "got {c}");
    }

    #[test]
    fn price_gen_cost_unknown_model_is_zero() {
        let prices = vec![price("openai", "gpt-4o", 2.5, 10.0)];
        assert!(approx(price_gen_cost(&prices, "google", "gemini", Some(10), Some(10)), 0.0));
        // None token counts count as zero.
        assert!(approx(price_gen_cost(&prices, "openai", "gpt-4o", None, None), 0.0));
    }

    #[test]
    fn value_to_text_unwraps_strings_else_json() {
        assert_eq!(value_to_text(&json!("hello")), "hello");
        assert_eq!(value_to_text(&json!(42)), "42");
        assert_eq!(value_to_text(&json!({ "a": 1 })), r#"{"a":1}"#);
    }

    #[test]
    fn short_takes_first_eight_chars() {
        assert_eq!(short("0123456789abcdef"), "01234567");
        assert_eq!(short("abc"), "abc"); // shorter than 8 → whole string
        assert_eq!(short(""), "");
    }

    #[test]
    fn run_status_verdicts() {
        assert_eq!(run_status(None, 0.9), "no_baseline");
        assert_eq!(run_status(Some(0.8), 0.9), "passed");
        assert_eq!(run_status(Some(0.8), 0.7), "regressed");
        // Equal-to-baseline is not a regression (slack absorbs float noise).
        assert_eq!(run_status(Some(0.8), 0.8), "passed");
    }

}
