# Performance Optimizer — Benchmark Suites & Runs

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. Compare mode judges targets strictly serially — concurrency capped by case count, not `--jobs`
- **Severity**: High
- **Category**: serial-await
- **File**: `crates/runner/src/compare.rs:155-175` (`for t in targets` … per-target `parallel_map(cases.len(), jobs, …)`)
- **Scenario**: A comparison benchmark with 6 targets × 8 cases, run with `--jobs 24` to exploit the judge/gen provider's rate budget. Each cell is a `generate` + `judge_output` round-trip (network-bound, often multiple seconds).
- **Root cause**: The `(target, case)` grid is never flattened. `parallel_map` is invoked **once per target**, over that target's cases only, and targets are consumed by a plain sequential `for`. So in-flight concurrency is bounded by `min(jobs, cases_per_target)`, and the whole grid runs as `n_targets` back-to-back barriers. With cases (8) < jobs (24), only 8-way parallelism is ever used and the 6 targets run one after another — ~6 sequential batch-times instead of `ceil(48/24)=2`.
- **Impact**: Up to `n_targets`× wall-clock inflation whenever `cases_per_target ≤ jobs` (the common case for focused compare suites). At 3 s/cell that grid is ~48 s serial vs ~6 s flattened. No extra provider load — same total calls, same `jobs` ceiling, just kept saturated.
- **Fix sketch**: Build one flat `Vec<(target_idx, case_idx)>`, run a single `parallel_map(grid.len(), jobs, …)` returning `(target_idx, Cell)`, then bucket cells by target and run the existing per-target fold/print/post loop unchanged. Concurrency stays bounded at `jobs`; only the barrier between targets is removed.
- **Trade-offs**: Folding/printing must be reordered after the parallel phase (already true within a target); the per-target `/v1/benchmark-runs` post stays sequential after the compute. Minor restructure, no behavior change to recorded numbers.

## 2. `dataset_ref` items are fetched and judged with no ceiling — a large stored dataset silently fans out thousands of paid LLM calls
- **Severity**: High
- **Category**: wasted-llm-call
- **File**: `crates/runner/src/bench.rs:57-69` (`get(.../items)` → every item becomes a `BenchmarkCase`); consumed by `run_compare`/`run_rubric_benchmark`/`run_simple`
- **Scenario**: A benchmark references a stored dataset (`dataset_ref`) that has grown to, say, 3,000 items. An operator runs a 4-target compare with `gen_samples=3`, `judge_samples=3`.
- **Root cause**: `GET /v1/datasets/{ds}/items` is fetched with no `limit`/pagination and every returned item is turned into a case. Compare then issues `targets × items × (gen_samples + judge_samples)` provider calls with no cap, no cost estimate, and no confirmation gate. Here: `4 × 3000 × (3+3) = 72,000` billed calls from one command. Nothing in the run path bounds or previews this.
- **Impact**: Direct, unbounded spend proportional to dataset size × targets × samples. At even $0.0005/judge+gen call that single invocation is ~$36; with a frontier judge model it is easily 10–100×. A mistakenly-large dataset or an accidental re-run is a real money event, not a slowdown.
- **Fix sketch**: (a) Pass a `--max-cases` (default sane cap, e.g. 200) into the items fetch as `?limit=` and truncate; (b) before dispatch, compute and print the projected call count = `targets × min(cases,cap) × (ng+samples)` and require `--yes`/confirmation (or `--budget` in $) when it exceeds a threshold. Fetching all rows then judging all is the expensive default; make the cap the default.
- **Trade-offs**: A cap can silently shrink an intentionally large sweep — mitigate by printing "judged N of M items (capped)" so truncation is visible, and let `--max-cases 0` opt out.

## 3. No intra-run checkpoint/resume — completed judging is forfeited on any post/fetch error and fully re-paid on retry
- **Severity**: Medium
- **Category**: no-resume
- **File**: `crates/runner/src/bench.rs:107-167` (simple) and `crates/runner/src/compare.rs:173-287`
- **Scenario**: A long compare run finishes all generation+judging (the entire LLM bill is already spent), then the final `post(.../v1/benchmark-runs)?` — or, in simple mode, an intermediate `post(.../v1/scores)?` — fails on a transient API hiccup. Or the process is killed near the end.
- **Root cause**: The run is not idempotent or resumable. Simple mode does all judging up front in `parallel_map` (bench.rs:107), then the sequential fold posts scores with `?` (bench.rs:166) and the run with `?` (bench.rs:201); compare posts the run with `?` (compare.rs:287) after all cells are judged. Any of these errors aborts the command with **no persisted run**. There is no keying of prior `(provider, model, system_prompt, input)` generations or `(judge, output)` verdicts, so the retry regenerates and re-judges every case from zero. Per-case scores in compare are best-effort (`let _ = post`), so they may survive, but nothing on the run path reads them back to skip already-judged work.
- **Impact**: A failure in the cheap bookkeeping tail throws away the expensive head. Retrying a `72k`-call-class run (see #2) re-pays the full LLM bill for a dropped HTTP response. Bounded to "cost of one run per failure," but that cost can be large and the trigger (one flaky POST) is common.
- **Fix sketch**: Persist the run row first (status `running`) and make judging resumable — before generating a cell, look up existing per-case scores for `(benchmark_id, run_id, target, case)` and skip judged cells; retry the terminal run-post with backoff instead of `?`-aborting. Minimum viable: wrap the final run-post in a bounded retry so completed judging is never lost to a single transient error.
- **Trade-offs**: True resume needs a stable run identity and a score-lookup key (schema-touching); the retry-the-final-post mitigation is small and captures most of the value.
