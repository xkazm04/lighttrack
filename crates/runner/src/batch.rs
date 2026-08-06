//! Packing cases into judge batches, and judging them.
//!
//! Batching is opt-in (`--batch N`) and its unit is "up to N cases, or fewer if they are large".
//! A fixed count alone is not safe: one case with a 200 KB output would blow the context and take
//! its whole batch down with it, so the packer also honours a size budget and a case that exceeds
//! the budget on its own is judged alone rather than dropped.
//!
//! The judged result is a `Vec<CaseResult>` in case order — exactly what the unbatched loop
//! produces — so printing, score posting and the scorecard downstream cannot tell the difference
//! except through the `batch_size` stamp each verdict carries.

use lighttrack_core::BenchmarkCase;
use lighttrack_core::Rubric;
use lighttrack_engine::{run_rubric_batch, run_rubric_judge, BatchCase, EngineConfig};

use crate::rubric::CaseResult;
use crate::runctl::RunControl;
use crate::util::parallel_map;

/// Roughly four characters per token. The packer only needs the right order of magnitude: it is
/// protecting against a case that is 100× the others, not tuning to the token.
const CHARS_PER_TOKEN: usize = 4;

/// Default ceiling on the case content in one batch, in characters (~50k tokens). Sized to leave
/// the model plenty of room for the rubric, the boundary contract and N verdicts' worth of output.
const DEFAULT_MAX_CHARS: usize = 200_000;

/// How much of a batch's budget a case consumes.
fn case_chars(c: &BenchmarkCase) -> usize {
    c.input.len()
        + c.expected.as_deref().map_or(0, str::len)
        + c.output.as_deref().map_or(0, str::len)
}

/// Group the indices of judgeable cases into batches of at most `max_cases`, each under the size
/// budget. Cases without an output never enter a batch — they are not judged at all.
///
/// A case larger than the whole budget still gets its own batch: refusing to judge a big case, or
/// silently truncating it, would be a worse answer than spending one call on it.
pub(crate) fn plan(cases: &[BenchmarkCase], max_cases: usize, max_chars: usize) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut cur_chars = 0usize;
    for (i, c) in cases.iter().enumerate() {
        if c.output.is_none() {
            continue;
        }
        let sz = case_chars(c);
        let full = cur.len() >= max_cases.max(1);
        let over = !cur.is_empty() && cur_chars + sz > max_chars;
        if full || over {
            groups.push(std::mem::take(&mut cur));
            cur_chars = 0;
        }
        cur.push(i);
        cur_chars += sz;
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    groups
}

/// The size budget, overridable for operators whose judge model has a smaller or larger window.
pub(crate) fn max_chars_from_env() -> usize {
    std::env::var("LIGHTTRACK_BATCH_MAX_CHARS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_CHARS)
}

/// Estimated tokens of case content in a batch — reported so an operator can see what was packed.
pub(crate) fn est_tokens(cases: &[BenchmarkCase], group: &[usize]) -> usize {
    group.iter().map(|&i| case_chars(&cases[i])).sum::<usize>() / CHARS_PER_TOKEN
}

/// Judge every case in `cases` in batches of at most `batch`, returning one [`CaseResult`] per case
/// in input order.
///
/// A batch whose call fails as a whole (transport, or a response that attributed nothing) is retried
/// **unbatched**, so turning batching on can lose throughput but never cases. A case the batch simply
/// did not answer fails on its own and is not retried — the engine already gave it a repair re-ask.
#[allow(clippy::too_many_arguments)]
pub(crate) fn judge_batched(
    engine: &EngineConfig,
    jp: &str,
    jm: &str,
    rubric: &Rubric,
    cases: &[BenchmarkCase],
    samples: u32,
    jobs: usize,
    batch: usize,
    ctl: &RunControl,
) -> Vec<CaseResult> {
    let groups = plan(cases, batch, max_chars_from_env());
    let n_cases = cases.len();
    if let Some(first) = groups.first() {
        println!(
            "  batching: {} call(s) for {} judgeable case(s) (largest batch {}, ~{} tok)",
            groups.len(),
            groups.iter().map(Vec::len).sum::<usize>(),
            groups.iter().map(Vec::len).max().unwrap_or(0),
            est_tokens(cases, first)
        );
    }

    // Batches run concurrently up to `jobs`; the engine's per-batch sample loop stays sequential so
    // total concurrency stays bounded at --jobs, matching the unbatched path's contract.
    let judged: Vec<Vec<(usize, CaseResult)>> = parallel_map(groups.len(), jobs, |g| {
        let group = &groups[g];
        if ctl.cancelled() {
            return group.iter().map(|&i| (i, CaseResult::NoOutput)).collect();
        }
        let items: Vec<BatchCase<'_>> = group
            .iter()
            .map(|&i| BatchCase {
                input: &cases[i].input,
                expected: cases[i].expected.as_deref(),
                output: cases[i].output.as_deref().unwrap_or(""),
            })
            .collect();
        let out = match run_rubric_batch(engine, jp, jm, rubric, &items, samples, 1) {
            Ok(per_case) => group
                .iter()
                .zip(per_case)
                .map(|(&i, r)| {
                    let cr = match r {
                        Ok(o) => CaseResult::Judged(Box::new(o)),
                        Err(e) => CaseResult::Errored(e.to_string()),
                    };
                    (i, cr)
                })
                .collect(),
            // The batch itself died. Fall back to one call per case so a batching failure costs
            // throughput, never coverage.
            Err(e) => {
                eprintln!(
                    "  batch of {} failed ({e}); retrying those cases unbatched",
                    group.len()
                );
                group
                    .iter()
                    .map(|&i| {
                        let cr = judge_one(engine, jp, jm, rubric, &cases[i], samples, ctl);
                        (i, cr)
                    })
                    .collect()
            }
        };
        for _ in 0..group.len() {
            ctl.tick(n_cases);
        }
        out
    });

    // Rebuild case order. A case with no output was never planned into a group and stays NoOutput.
    let mut results: Vec<CaseResult> = (0..n_cases).map(|_| CaseResult::NoOutput).collect();
    for (i, r) in judged.into_iter().flatten() {
        results[i] = r;
    }
    results
}

/// One case, judged alone — the unbatched path, reused by the fallback.
fn judge_one(
    engine: &EngineConfig,
    jp: &str,
    jm: &str,
    rubric: &Rubric,
    case: &BenchmarkCase,
    samples: u32,
    ctl: &RunControl,
) -> CaseResult {
    if ctl.cancelled() {
        return CaseResult::NoOutput;
    }
    let Some(output) = case.output.as_deref() else {
        return CaseResult::NoOutput;
    };
    match run_rubric_judge(
        engine,
        jp,
        jm,
        rubric,
        &case.input,
        case.expected.as_deref(),
        output,
        samples,
        1,
    ) {
        Ok(o) => CaseResult::Judged(Box::new(o)),
        Err(e) => CaseResult::Errored(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(output: Option<&str>, size: usize) -> BenchmarkCase {
        BenchmarkCase {
            input: "x".repeat(size),
            expected: None,
            output: output.map(|o| o.to_string()),
        }
    }

    #[test]
    fn packs_up_to_the_case_limit() {
        let cases: Vec<BenchmarkCase> = (0..7).map(|_| case(Some("a"), 1)).collect();
        let groups = plan(&cases, 3, 1_000_000);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0], vec![0, 1, 2]);
        assert_eq!(groups[2], vec![6]);
    }

    #[test]
    fn cases_without_output_are_never_batched() {
        let cases = vec![case(Some("a"), 1), case(None, 1), case(Some("a"), 1)];
        let groups = plan(&cases, 10, 1_000_000);
        assert_eq!(
            groups,
            vec![vec![0, 2]],
            "an unjudgeable case must not take up a slot or shift indices"
        );
    }

    #[test]
    fn the_size_budget_splits_a_batch_the_count_would_have_allowed() {
        let cases: Vec<BenchmarkCase> = (0..4).map(|_| case(Some("a"), 100)).collect();
        let groups = plan(&cases, 10, 250);
        assert_eq!(
            groups.len(),
            2,
            "10 cases fit by count but only 2 fit the budget: {groups:?}"
        );
    }

    #[test]
    fn an_oversized_case_is_judged_alone_not_dropped() {
        let cases = vec![
            case(Some("a"), 10),
            case(Some("a"), 10_000),
            case(Some("a"), 10),
        ];
        let groups = plan(&cases, 10, 100);
        let flat: Vec<usize> = groups.iter().flatten().copied().collect();
        assert_eq!(flat, vec![0, 1, 2], "every judgeable case must be planned");
        assert!(
            groups.iter().any(|g| g == &vec![1]),
            "the oversized case gets its own call: {groups:?}"
        );
    }
}
