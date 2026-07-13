//! Bounded, ordered parallel map over a blocking workload. The engine stays sync/blocking — no async
//! rewrite — so concurrency is a scoped thread pool that pulls indices off a shared counter and writes
//! each result into its own slot. Results come back in index order, so aggregation is byte-identical to
//! the sequential path; `jobs <= 1` (or `n <= 1`) runs inline with no threads spawned.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Apply `f` to each `0..n` with at most `jobs` worker threads, returning results in index order.
/// `f` must be `Sync` (shared across workers); `T` must be `Send`. Deterministic: worker scheduling
/// never affects output order, so `jobs == 1` and `jobs == N` yield identical `Vec`s.
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
    // Each slot is written exactly once, by whichever worker claims that index. A Mutex keeps this
    // safe without unsafe cell juggling; the lock is held only for the O(1) store, never across `f`
    // (the expensive LLM call), so contention is negligible against network/subprocess latency.
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

#[cfg(test)]
mod tests {
    use super::parallel_map;

    #[test]
    fn preserves_index_order_across_job_counts() {
        let seq = parallel_map(20, 1, |i| i * i);
        let par = parallel_map(20, 8, |i| i * i);
        let expected: Vec<usize> = (0..20).map(|i| i * i).collect();
        assert_eq!(seq, expected);
        assert_eq!(par, expected, "parallel result must match sequential order");
    }

    #[test]
    fn handles_empty_and_single() {
        assert_eq!(parallel_map(0, 4, |i: usize| i), Vec::<usize>::new());
        assert_eq!(parallel_map(1, 4, |i| i + 1), vec![1]);
    }
}
