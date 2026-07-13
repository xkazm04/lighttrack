//! Small shared helpers: percentiles, dimension means, token-priced cost, claude resolution.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};

use lighttrack_core::ModelPriceRow;

/// Comma-join a set of labels for a one-line log/warning.
pub(crate) fn join_csv(items: &BTreeSet<String>) -> String {
    items.iter().cloned().collect::<Vec<_>>().join(", ")
}

/// Attach collected missing-price warnings to a run report so they persist with the run (queryable),
/// rather than scrolling past on stderr. No-op when nothing was unpriced.
pub(crate) fn add_price_warnings(report: &mut Value, warnings: &BTreeSet<String>) {
    if warnings.is_empty() {
        return;
    }
    let models: Vec<Value> = warnings.iter().map(|m| json!(m)).collect();
    if let Some(obj) = report.as_object_mut() {
        obj.insert("price_warnings".into(), json!(models));
    }
}

/// `now` as the fixed-width RFC3339(Nanos, Z) the store persists (see store `codec::fmt_ts`). Runs
/// stamp `finished_at` with this so a recorded run's duration is knowable and string-orderable.
pub(crate) fn now_ts() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Apply `f` to each `0..n` with at most `jobs` scoped worker threads, returning results in index
/// order. The heavy work (LLM generation/judging) is blocking, so a bounded thread pool cuts a
/// benchmark/compare/score/calibrate run's wall-clock with zero effect on aggregation: results come
/// back ordered, so `jobs == 1` and `jobs == N` are byte-identical. Side effects (printing, POSTing
/// scores) must stay in the caller's sequential fold, never inside `f`.
pub(crate) fn parallel_map<T, F>(n: usize, jobs: usize, f: F) -> Vec<T>
where
    F: Fn(usize) -> T + Sync,
    T: Send,
{
    let jobs = jobs.clamp(1, n.max(1));
    if jobs == 1 || n <= 1 {
        return (0..n).map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let slots: Mutex<Vec<Option<T>>> = Mutex::new((0..n).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let v = f(i);
                if let Ok(mut guard) = slots.lock() {
                    guard[i] = Some(v);
                }
            });
        }
    });
    slots
        .into_inner()
        .unwrap_or_default()
        .into_iter()
        .map(|o| o.expect("every index is assigned exactly once"))
        .collect()
}

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

/// Roll several per-target verdicts up to one run-level verdict: `regressed` if any target
/// regressed, else `passed` if any held against a baseline, else `no_baseline`. Used by compare mode
/// so the whole comparison has a single honest headline status, not just per-target rows.
pub(crate) fn aggregate_status(statuses: &[&str]) -> &'static str {
    if statuses.iter().any(|s| *s == "regressed") {
        "regressed"
    } else if statuses.iter().any(|s| *s == "passed") {
        "passed"
    } else {
        "no_baseline"
    }
}

/// Cost of a call from the DB price book, plus whether the model was actually found in the book.
/// `priced=false` means there was no book entry, so the token-based cost silently fell back to 0 —
/// callers surface this as a run-report warning instead of recording a misleadingly-cheap run.
pub(crate) fn price_gen_cost_checked(
    prices: &[ModelPriceRow],
    provider: &str,
    model: &str,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
) -> (f64, bool) {
    match prices.iter().find(|p| p.provider == provider && p.model == model) {
        Some(p) => (
            (input_tokens.unwrap_or(0) as f64) * p.input_per_mtok / 1_000_000.0
                + (output_tokens.unwrap_or(0) as f64) * p.output_per_mtok / 1_000_000.0,
            true,
        ),
        None => (0.0, false),
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
    price_gen_cost_checked(prices, provider, model, input_tokens, output_tokens).0
}

/// A call's cost with a book fallback that flags a missing price. When the provider already returned
/// a `$` cost we trust it (priced=true); otherwise we price by tokens from the book and report
/// whether the model was present. Returns `(cost, priced)`; `priced=false` ⇒ collect a warning.
pub(crate) fn cost_or_book(
    provider_cost: Option<f64>,
    prices: &[ModelPriceRow],
    provider: &str,
    model: &str,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
) -> (f64, bool) {
    match provider_cost {
        Some(c) => (c, true),
        None => price_gen_cost_checked(prices, provider, model, input_tokens, output_tokens),
    }
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
    fn aggregate_status_prioritizes_regression() {
        assert_eq!(aggregate_status(&["passed", "regressed", "no_baseline"]), "regressed");
        assert_eq!(aggregate_status(&["passed", "no_baseline"]), "passed");
        assert_eq!(aggregate_status(&["no_baseline", "no_baseline"]), "no_baseline");
        assert_eq!(aggregate_status(&[]), "no_baseline");
    }

    #[test]
    fn parallel_map_preserves_order_and_matches_sequential() {
        let seq = parallel_map(25, 1, |i| i * 3);
        let par = parallel_map(25, 8, |i| i * 3);
        let expected: Vec<usize> = (0..25).map(|i| i * 3).collect();
        assert_eq!(seq, expected);
        assert_eq!(par, expected, "parallel result must match sequential order byte-for-byte");
        assert_eq!(parallel_map(0, 4, |i: usize| i), Vec::<usize>::new());
    }

    #[test]
    fn price_gen_cost_checked_flags_missing() {
        let prices = vec![price("openai", "gpt-4o", 2.5, 10.0)];
        let (cost, priced) = price_gen_cost_checked(&prices, "openai", "gpt-4o", Some(1_000_000), None);
        assert!(approx(cost, 2.5) && priced);
        let (cost, priced) = price_gen_cost_checked(&prices, "google", "gemini", Some(10), Some(10));
        assert!(approx(cost, 0.0) && !priced);
    }

    #[test]
    fn cost_or_book_trusts_provider_then_falls_back() {
        let prices = vec![price("openai", "gpt-4o", 2.5, 10.0)];
        // Provider gave a $ cost → trusted verbatim, priced=true, book untouched.
        let (cost, priced) = cost_or_book(Some(0.123), &prices, "who", "ever", Some(1), Some(1));
        assert!(approx(cost, 0.123) && priced);
        // No provider cost, model in book → priced from tokens.
        let (cost, priced) = cost_or_book(None, &prices, "openai", "gpt-4o", Some(1_000_000), None);
        assert!(approx(cost, 2.5) && priced);
        // No provider cost, model absent → 0 cost and a warning flag.
        let (cost, priced) = cost_or_book(None, &prices, "x", "y", Some(1), Some(1));
        assert!(approx(cost, 0.0) && !priced);
    }

    #[test]
    fn now_ts_is_fixed_width_nanos_utc() {
        let s = now_ts();
        assert!(s.ends_with('Z'));
        assert_eq!(s.len(), "2026-05-31T00:07:14.110948400Z".len());
    }
}
