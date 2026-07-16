# Feature Scout — Judge Pairwise, Parsing & Provider Pool

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. Pairwise ranking ships with no statistical confidence — the one mode that skips `stats.rs`

- **Severity**: High
- **Category**: capability-gap
- **File**: `crates/runner/src/pairwise.rs:149-160, 196-210, 256-280` (vs `crates/runner/src/stats.rs:57-113`)
- **Scenario**: A user runs `--pairwise` over 3 targets and 4 cases. That is 12 games. The printed ranking says `1. gpt-4o win_rate=0.625`, `2. claude win_rate=0.500`. They ship gpt-4o. But 12 games with ties-as-half-wins puts a ±0.28 Wilson interval around 0.625 — the ranking is indistinguishable from a coin flip, and nothing on screen says so. This is precisely the "is model A better than model B?" question the feature exists to answer, and it answers it with a point estimate presented as a verdict.
- **Root cause**: `crates/runner/src/stats.rs` already implements `Summary::of` / `ci95` / `significance_verdict` / `annotate_significance`, and `bench.rs:174`, `rubric.rs:219`, and `compare.rs:256` all call them. `pairwise.rs` imports none of it. `print_ranking` (line 197) prints raw `win_rate()`; `post_run` (line 267) posts `"win_rate": standings[i].win_rate()` with no `n`/`stderr`/`ci95` block and hardcodes `"status": "no_baseline"`. The comment at line 273 justifies `no_baseline` on the grounds that "there is no baseline to regress against" — true, but it conflates *no baseline* with *no significance*: the real question is whether target i beats target j, and that is a two-sample question the file never asks.
- **Impact**: This is the competitive surface against LangSmith/Braintrust. A ranking with an honest "no significant difference between #1 and #2 (n=12, need ~60 games)" is a *more* trustworthy product than a bare leaderboard, and it directly upsells more cases/more runs. It also stops users making six-figure model decisions on noise — the exact failure mode `significance_verdict`'s doc comment ("a noisy 3-case run can't trip it as easily as a 3000-case run") was written to prevent everywhere else.
- **Fix sketch**:
  1. Add `wilson_interval(wins: f64, n: usize) -> (f64, f64)` to `stats.rs` beside `ci95` (same pure/unit-tested shape). Ties count as 0.5 wins, matching `Standing::win_rate`.
  2. In `print_ranking`, print `win_rate=0.625 [0.35, 0.85] n=8` per target.
  3. Add a pairwise-native head-to-head verdict: for each pair (i,j), a two-sided sign test over that pair's games → `better | worse | inconclusive`. `beats[i][j]` vs `beats[j][i]` already holds the counts.
  4. In `post_run`, add `"ci95"` per ranking row and a `"pairwise_verdicts"` array; emit a `caveats` entry (`"underpowered: n=12 games"`) reusing the `SMALL_N` idiom from `stats.rs:14`.
  5. Print a one-line headline: `no significant separation among top 2 (p>0.05)`.
- **Trade-offs**: Round-robin games are not independent across cases (same candidate output reused in multiple pairs), so a strict sign test is mildly optimistic. Acceptable — document it as a caveat; it is still enormously better than silence. Additive JSON only, so old runs are unaffected.

## 2. Every game's judge reasoning is generated, paid for, and thrown away

- **Severity**: High
- **Category**: half-implemented
- **File**: `crates/runner/src/pairwise.rs:140-148` (drop site), `crates/engine/src/pairwise.rs:39, 89-93` (produced), `crates/runner/src/pairwise.rs:267-271` (report)
- **Scenario**: The matrix says T2 beat T1 four times. The user asks the only follow-up question that matters — *why?* — and there is no answer anywhere: not on stdout, not in the posted run, not in the DB. They rerun the pair by hand in a chat window to find out. Same for bias: the run reports `positional_ties(bias)=3` but not *which* pairs were biased, so an operator cannot tell whether one flaky target attracts all the bias or it is spread evenly.
- **Root cause**: `PairwiseOutcome` carries a fully-populated `reasoning: String` (`engine/src/pairwise.rs:39`), and `assemble` goes out of its way to build a good one — on bias it composes both orders' rationales (`engine/src/pairwise.rs:89-93`). The runner's fold then does `winners.push((o.winner, o.position_bias))` (line 147), discarding `o.reasoning` on the floor. Nothing else in the repo reads it (`PairwiseOutcome.reasoning` has no other consumer). The posted `report` (line 267) is aggregate-only: `ranking`, `beats_matrix`, `n_games`, `positional_bias_ties` — no per-game rows, so the case index, the pair, the winner, and the reasoning are all structurally unrecoverable after the run. The output tokens for that reasoning are billed on all `2 × n_games` judge calls and counted into `judge_cost` (line 143).
- **Impact**: Turns a leaderboard into evidence. "T2 beat T1 on case 3: *T1 hallucinated the refund window*" is the artifact a user screenshots for their team and the thing that makes an eval tool sticky — it is what LangSmith/Braintrust run traces sell. It is already 100% paid for; only the plumbing is missing. It also makes bias debuggable and gives the UI something to render, and it is the natural drill-down target for finding #1's pair verdicts.
- **Fix sketch**:
  1. Widen the fold's accumulator from `(PairwiseWinner, bool)` to a `Game { case: usize, i: usize, j: usize, winner, position_bias, reasoning: String, cost_usd, tokens }`. `tally` keeps taking the narrow tuple — feed it `games.iter().map(|g| (g.winner, g.bias))` so its existing unit tests stand.
  2. Add `"games": [...]` to the `report` JSON in `post_run`, one row per game with the reasoning. Cap reasoning at ~500 chars to bound the payload.
  3. Print the top few decisive rationales under the matrix, or all of them behind a `--verbose`/`--explain` flag.
  4. Once rows exist, add a bias breakdown: per-pair `positional_ties` instead of one global `bias_count`.
- **Trade-offs**: Report payload grows roughly linearly with `n_games` (a 5-target × 20-case run is 200 games ≈ 100KB with capped reasoning). Mitigate with the char cap and, if needed, a `--no-explain` opt-out. No behavior change to scoring.

## 3. Pairwise cannot be sampled k times, and one bad judge response destroys the whole round-robin

- **Severity**: Medium
- **Category**: capability-gap
- **File**: `crates/engine/src/pairwise.rs:81-88, 130-149`; `crates/runner/src/pairwise.rs:141-142`
- **Scenario**: A 5-target × 10-case run has already spent $4 generating 50 candidates and judged 90 of 100 games. Game 91's judge rambles past its repair re-ask. `assemble` errors; `let o = outcome?` propagates; the process exits with a parse error and **zero output** — no matrix, no ranking, no partial result, and the run is never posted. Every dollar already spent is gone. Separately, a user who wants to trust a close call cannot ask the pairwise judge for 3 samples per order the way they can for a rubric.
- **Root cause**: Two asymmetries against the rubric judge, which solved both problems already:
  - **No sampling.** `run_rubric_judge` takes `samples: u32, jobs: usize` and returns `agreement` + `parse_failures` (`engine/src/judge.rs:167-182`). `run_pairwise` (`engine/src/pairwise.rs:130`) takes neither — it is hardwired to exactly two calls, one per order, with no `agreement` field on `PairwiseOutcome`. The `pool::parallel_map` that powers rubric self-consistency (`judge.rs:197`) is right there and unused by pairwise.
  - **No graceful degradation.** `parse.rs`'s whole design contract is that an unparseable sample "surfaces as `value: None`" rather than a fabricated score (`parse.rs:2-4`). The rubric judge honors this: `aggregate` drops the sample, counts it in `parse_failures`, and only errors if *every* sample failed (`judge.rs:259-265`). `assemble` (`engine/src/pairwise.rs:82-87`) instead converts a single `value: None` straight into a hard `EngineError::Parse` — and the runner's fold turns that one game's failure into a whole-run abort at line 142.
- **Impact**: Closing the second half is cheap and makes long round-robins survivable — the run that today loses $4 instead prints its matrix with `unjudged_games=1` noted. Closing the first half gives pairwise the self-consistency story rubric already has, plus a per-game agreement metric that feeds finding #1's confidence math directly (a game where 3 samples split 2-1 is weaker evidence than 3-0).
- **Fix sketch**:
  1. *Resilience first (small).* Make a dropped game non-fatal: have the runner's fold partition `outcomes` into judged games and failures, count `unjudged_games`, and abort only if **all** games failed — mirroring `aggregate`'s all-failed guard. Print and post the count. This is a contained change to `runner/src/pairwise.rs:141-148`.
  2. *Then sampling.* Add `samples: u32` to `run_pairwise`; run `samples` × 2 calls through `pool::parallel_map`, majority-vote the per-order verdicts, and add `agreement: f64` + `parse_failures: u32` to `PairwiseOutcome`. Reuse `Standing`-style folding; keep `combine`/`unswap` untouched so their tests hold.
  3. Thread a `--pairwise-samples` flag through `run_pairwise_matrix`, defaulting to 1 (= today's exact behavior, two calls).
- **Trade-offs**: Sampling multiplies judge cost by `2 × samples` per game on top of an already-quadratic `n_targets²` game count — hence default 1 and an explicit opt-in. Step 1 alone carries no cost and no API change and is worth doing regardless of whether step 2 ever ships.

---

### Checked and deliberately not filed

- **Elo / Bradley-Terry ratings.** `runner/src/pairwise.rs:4` names them an explicit non-goal. Verified the call is correct rather than stale: the runner judges a *complete* round-robin (`pairwise.rs:116-125` enumerates every unordered pair), where strength-of-schedule is uniform and win-rate is a sufficient statistic. Elo earns its keep only with sparse/incomplete pairings, which this scheduler cannot produce. Finding #1 (confidence intervals) is the higher-value use of the same effort.
- **`pool.rs` / `retry.rs`.** Read in full; both are complete and well-tested against their stated contracts (deterministic index-ordered `parallel_map`; typed-variant retry classification with jittered backoff). No feature gap worth a finding — `pool.rs`'s unused-by-pairwise concurrency is folded into finding #3 rather than filed separately.
- **`extract_json_object`'s outermost-brace strategy** (`parse.rs:12-16`). Naive against prose containing stray braces, but that is a robustness edge, not a feature gap, and the repair re-ask covers the realistic failure.
