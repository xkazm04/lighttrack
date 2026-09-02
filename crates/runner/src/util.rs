//! Small shared helpers: percentiles, dimension means, token-priced cost, claude resolution.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};

use lighttrack_core::{ModelPriceRow, PriceBook, TokenUsage};

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
/// regressed, else `cancelled`/`partial` if any target's cases were cut short (an operator stopped
/// the run, or its budget ceiling did), else `passed` if any held against a baseline, else
/// `no_baseline`. Used by compare mode so the whole comparison has
/// a single honest headline status, not just per-target rows.
///
/// A real regression outranks partiality on purpose: a target that regressed on the cases it *did*
/// run is an actionable finding, and the halt is still recorded per target and on the run summary.
/// But a halted comparison can never roll up to `passed` — that is the claim it hasn't earned.
pub(crate) fn aggregate_status(statuses: &[&str]) -> &'static str {
    if statuses.contains(&"regressed") {
        "regressed"
    } else if statuses.contains(&"cancelled") {
        "cancelled"
    } else if statuses.contains(&"partial") {
        "partial"
    } else if statuses.contains(&"passed") {
        "passed"
    } else {
        "no_baseline"
    }
}

/// Cost of a call from the DB price book, plus whether the model was actually found in the book.
/// `priced=false` means there was no book entry, so the token-based cost fell back to 0 — callers
/// surface this as a run-report warning instead of recording a misleadingly-cheap run.
///
/// **There is one pricing authority, and it is [`PriceBook`].** This function used to be a second
/// one: an exact `provider == p.provider && model == p.model` scan with a hand-rolled per-mtok
/// multiply. It therefore disagreed with the ingest path on three things the book resolves —
/// date-suffix families (`claude-haiku-4-5-20260101` → `claude-haiku-4-5`), batch/flex variants, and
/// prompt-length tiers — and disagreed *silently*, because an unresolved model returns `0.0` and a
/// zero looks like a cheap call rather than a missing price. That is the shape where a benchmark's
/// spend report and the product's own cost rollup quietly stop agreeing about the same call.
///
/// Building the book per call is O(rows) over a table of a few hundred entries, which is nothing
/// beside the LLM call being priced — and it is the price of having one authority rather than two.
pub(crate) fn price_gen_cost_checked(
    prices: &[ModelPriceRow],
    provider: &str,
    model: &str,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
) -> (f64, bool) {
    let book = PriceBook::from_rows(prices);
    let usage = TokenUsage {
        input: input_tokens.unwrap_or(0),
        output: output_tokens.unwrap_or(0),
        ..Default::default()
    };
    match book.cost_usd(provider, model, &usage) {
        Some(cost) => (cost, true),
        // Unpriced stays `(0.0, false)`: the caller's contract is "0 with a warning", never a
        // phantom cost. The distinction is the whole reason this returns a pair.
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

/// Stamp a run report's reproducibility as **two separate facts**: how pinned the candidate
/// *generation* was, and how pinned the *judging* of it was. A single stamp could only ever be as
/// honest as its weakest half, and until generation was pinned at all the reported `exact` described
/// the judge while the thing being judged was redrawn on every run. `determinism` stays the
/// (unchanged-shape) headline and is now the **weaker** of the two — so it can never overstate — with
/// `determinism_detail` naming which half is the limit. A `None` half means that half didn't happen
/// (rubric/simple modes judge outputs supplied by the caller, and generate nothing).
pub(crate) fn stamp_determinism(
    report: &mut Value,
    generation: Option<lighttrack_engine::Determinism>,
    judging: Option<lighttrack_engine::Determinism>,
) {
    let overall = match (generation, judging) {
        (Some(g), Some(j)) => Some(g.weakest(j)),
        (Some(g), None) => Some(g),
        (None, Some(j)) => Some(j),
        (None, None) => None,
    };
    let Some(obj) = report.as_object_mut() else {
        return;
    };
    obj.insert("determinism".into(), json!(overall.map(|d| d.as_str())));
    obj.insert(
        "determinism_detail".into(),
        json!({
            "generation": generation.map(|d| d.as_str()),
            "judging": judging.map(|d| d.as_str()),
        }),
    );
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
    fn price(
        provider: &str,
        model: &str,
        input_per_mtok: f64,
        output_per_mtok: f64,
    ) -> ModelPriceRow {
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
        assert!(approx(
            price_gen_cost(&prices, "google", "gemini", Some(10), Some(10)),
            0.0
        ));
        // None token counts count as zero.
        assert!(approx(
            price_gen_cost(&prices, "openai", "gpt-4o", None, None),
            0.0
        ));
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
        assert_eq!(
            aggregate_status(&["passed", "regressed", "no_baseline"]),
            "regressed"
        );
        assert_eq!(aggregate_status(&["passed", "no_baseline"]), "passed");
        // A budget-halted target can never let the comparison roll up to `passed`.
        assert_eq!(aggregate_status(&["passed", "partial"]), "partial");
        assert_eq!(aggregate_status(&["passed", "cancelled"]), "cancelled");
        assert_eq!(aggregate_status(&["partial", "no_baseline"]), "partial");
        // …but a real regression on the cases that did run still outranks it.
        assert_eq!(aggregate_status(&["partial", "regressed"]), "regressed");
        assert_eq!(
            aggregate_status(&["no_baseline", "no_baseline"]),
            "no_baseline"
        );
        assert_eq!(aggregate_status(&[]), "no_baseline");
    }

    #[test]
    fn parallel_map_preserves_order_and_matches_sequential() {
        let seq = parallel_map(25, 1, |i| i * 3);
        let par = parallel_map(25, 8, |i| i * 3);
        let expected: Vec<usize> = (0..25).map(|i| i * 3).collect();
        assert_eq!(seq, expected);
        assert_eq!(
            par, expected,
            "parallel result must match sequential order byte-for-byte"
        );
        assert_eq!(parallel_map(0, 4, |i: usize| i), Vec::<usize>::new());
    }

    #[test]
    fn price_gen_cost_checked_flags_missing() {
        let prices = vec![price("openai", "gpt-4o", 2.5, 10.0)];
        let (cost, priced) =
            price_gen_cost_checked(&prices, "openai", "gpt-4o", Some(1_000_000), None);
        assert!(approx(cost, 2.5) && priced);
        let (cost, priced) =
            price_gen_cost_checked(&prices, "google", "gemini", Some(10), Some(10));
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
    fn determinism_stamp_never_overstates_the_weaker_half() {
        use lighttrack_engine::Determinism::{BestEffort, Exact, Sampled};
        // An exactly-pinned judge over a best-effort candidate is NOT an exactly reproducible run.
        let mut r = json!({});
        stamp_determinism(&mut r, Some(BestEffort), Some(Exact));
        assert_eq!(r["determinism"], json!("best-effort"));
        assert_eq!(r["determinism_detail"]["generation"], json!("best-effort"));
        assert_eq!(r["determinism_detail"]["judging"], json!("exact"));
        // Both pinned → exact.
        let mut r = json!({});
        stamp_determinism(&mut r, Some(Exact), Some(Exact));
        assert_eq!(r["determinism"], json!("exact"));
        // Deliberate multi-draw generation is the weakest claim of all.
        let mut r = json!({});
        stamp_determinism(&mut r, Some(Sampled), Some(Exact));
        assert_eq!(r["determinism"], json!("sampled"));
        // Judge-only modes (no generation) keep the judge's stamp verbatim.
        let mut r = json!({});
        stamp_determinism(&mut r, None, Some(Exact));
        assert_eq!(r["determinism"], json!("exact"));
        assert_eq!(r["determinism_detail"]["generation"], json!(null));
        // Nothing measured → claims nothing.
        let mut r = json!({});
        stamp_determinism(&mut r, None, None);
        assert_eq!(r["determinism"], json!(null));
    }

    #[test]
    fn now_ts_is_fixed_width_nanos_utc() {
        let s = now_ts();
        assert!(s.ends_with('Z'));
        assert_eq!(s.len(), "2026-05-31T00:07:14.110948400Z".len());
    }

    /// One pricing authority, held to it.
    ///
    /// The runner used to price with an exact `(provider, model)` string match and its own per-mtok
    /// multiply, so it disagreed with the ingest path on every model whose price the book resolves
    /// through a family, a variant, or a tier — and disagreed silently, because an unresolved model
    /// returns 0.0 and a zero reads as a cheap call rather than a missing price. These cases are the
    /// three resolutions the old scan could not do, each asserted against the book's own answer.
    #[test]
    fn the_runner_prices_through_the_one_price_book() {
        let rows = |pairs: &[(&str, f64, f64)]| -> Vec<ModelPriceRow> {
            pairs
                .iter()
                .map(|(m, i, o)| ModelPriceRow {
                    provider: "anthropic".into(),
                    model: (*m).into(),
                    input_per_mtok: *i,
                    output_per_mtok: *o,
                    cached_input_per_mtok: None,
                    effective_from: Utc::now(),
                    source_url: None,
                    verified_at: None,
                    note: None,
                })
                .collect()
        };
        let usage = |i: u64, o: u64| TokenUsage {
            input: i,
            output: o,
            ..Default::default()
        };
        let same = |prices: &[ModelPriceRow], model: &str, i: u64, o: u64| {
            let (runner, priced) =
                price_gen_cost_checked(prices, "anthropic", model, Some(i), Some(o));
            let book = PriceBook::from_rows(prices).cost_usd("anthropic", model, &usage(i, o));
            assert_eq!(
                priced,
                book.is_some(),
                "{model}: the runner and the book must agree on whether it is priced at all"
            );
            match book {
                Some(b) => assert!(
                    (runner - b).abs() < 1e-12,
                    "{model}: runner priced {runner}, the book says {b}"
                ),
                None => assert_eq!(runner, 0.0, "{model}: unpriced is 0.0 with priced=false"),
            }
            (runner, priced)
        };

        // 1. Date-suffix family: the book trims `-20260101`; the old exact scan found nothing and
        //    reported a free call.
        let book = rows(&[("claude-haiku-4-5", 1.0, 5.0)]);
        let (cost, priced) = same(&book, "claude-haiku-4-5-20260101", 1_000_000, 1_000_000);
        assert!(priced, "a dated model name must resolve to its family");
        assert!((cost - 6.0).abs() < 1e-12, "{cost}");

        // 2. Prompt-length tier: above the threshold the tiered row applies.
        let tiered = rows(&[("m", 1.0, 5.0), ("m@in>200000", 2.0, 10.0)]);
        let (small, _) = same(&tiered, "m", 1_000, 0);
        let (large, _) = same(&tiered, "m", 300_000, 0);
        assert!(
            large / 300.0 > small,
            "the tier must bite above its threshold: {small} vs {large}"
        );

        // 3. Genuinely unpriced stays 0.0 AND false — a phantom zero cost and a real free call must
        //    never be spelled the same way.
        let (cost, priced) = same(&book, "a-model-nobody-priced", 1_000_000, 0);
        assert_eq!((cost, priced), (0.0, false));
    }
}
