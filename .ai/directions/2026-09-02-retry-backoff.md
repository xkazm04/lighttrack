---
subject: software-engineering/retry-backoff
project: tracklight
raised_by: intake intake-portkey-0902 (peer comparison)
source: librarian/sources/2026-09-02-portkey-gateway.md
stage: the outbound provider call in crates/engine, and the target matrix in crates/runner that fans it out
size: 4 files / ~200 lines / M
status: accepted
---

## Why the scope implies it

`scope.does` says *"benchmark providers"*. Benchmarking providers means calling them in a matrix — `targets × cases × gen_samples × (1 + judge_samples)` (`docs\BENCHMARK_FRAMEWORK.md` §5a) — which means meeting rate limits and outages as the ordinary case, not the exceptional one. The retry lane is therefore load-bearing for the clause the scope leads with, and two of its decisions are currently unmade.

**tracklight emits the header it will not read.** `ApiError` carries a `retry_after` field (`crates\api\src\error.rs:121-138`); the shed path answers 503 with it (`crates\api\src\shed.rs:187-199`); the limit engine computes `retry_after_secs()` (`crates\core\src\limits.rs:388-390`). tracklight asserts to its own callers that a dependency's stated schedule outranks their guess. Its own outbound ladder never looks: `with_retry` takes `impl FnMut() -> Result<T>` (`crates\engine\src\retry.rs:35-47`), so the response — and the header on it — is gone before the retry decision is made. A 429 from Anthropic mid-matrix gets 200ms, 400ms, and `EngineError::RateLimited` (`retry.rs:11`, `:18-25`).

The peer reads three ordered spellings with two unit systems — `['retry-after-ms', 'x-ms-retry-after-ms', 'retry-after']` (`C:/t/portkey/src/globals.ts:7`), the last multiplied by 1000 at `src/handlers/retryHandler.ts:118` — and lets the stated delay **zero the remaining ladder** (`retryHandler.ts:104-146`).

**And no ladder here is bounded by a clock.** The peer holds a 60-second whole-request budget (`C:/t/portkey/src/globals.ts:5`) and resolves the collision explicitly: a stated delay `>=` the budget, or `>` what remains of it, **ends the ladder** — it does not shorten the wait and does not retry sooner — spending zero further attempts and reporting its own terminal state (`retryHandler.ts:104-146`; `src/handlers/handlerUtils.ts:1283-1288`). tracklight has a *dollar* ceiling with exactly this discipline (`crates\runner\src\budget.rs:100-153`, checked at a case boundary at `crates\runner\src\compare.rs:107-113`) and no wall-clock twin.

The fleet has already solved the request-level half, which is why this is a small direction rather than a research one: `C:\Users\kazda\kiro\pumper\crates\engine-http\src\lib.rs:917-930` refuses to truncate a stated wait — *"Truncating the sleep instead would be worse than failing — it would retry earlier than the server asked, which is the one thing politeness must never do"* — and returns a distinct `budget_exhausted` error instead (`:566-597`). That is the reference implementation.

**The matrix has no health filter.** `crates\runner\src\compare.rs:368-390` flattens every `(target × case)` cell into one `parallel_map` and attempts all of them; a failing cell records `error_msg` and increments `errored` (`:135`, `:182`, `:459-462`) and the next case calls the same dead provider again. A ten-target, sixty-case run against a provider that is down spends sixty calls learning it once. The peer prunes open targets from the candidate list before dispatch (`C:/t/portkey/src/handlers/handlerUtils.ts:646-658`) — and, crucially, **only when at least one healthy candidate remains** (`:655-657`), because refusing to route when everything looks sick turns a partial outage into a total one. tracklight already holds that second rule, one crate over: `Semaphore::new(max_concurrent_investigations.max(1))`, *"so a misconfigured `0` doesn't wedge every investigation"* (`crates\responder\src\breaker.rs:56-57`).

## What the first context contains

Two changes in `crates\engine`, one in `crates\runner`.

**`crates\engine\src\retry.rs` — the ladder learns two things.** `with_retry`'s closure signature widens so the failing response's stated delay reaches the scheduler; an ordered accept-list of header spellings resolves it (three names, two unit systems, first match wins); the stated delay replaces the computed step. And the loop takes a deadline: `MAX_TRIES` (`:11`) stops being the only bound. Jitter stays exactly as it is (`:50-66`) — it is the thing the peer got wrong.

**The collision rule, copied from pumper not from portkey.** A stated wait that does not fit the remaining deadline ends the ladder rather than being truncated, and it returns its own `EngineError` variant so a run report can say *"the provider asked for longer than we had"* rather than *"rate limited"*. `EngineError` (`crates\engine\src\lib.rs:47-84`) already separates ten failure modes on exactly this principle; this is an eleventh.

**`crates\runner\src\compare.rs` — a health filter over the matrix**, beside `compute_cell` (`:71-206`). A target that has failed N consecutive cells is skipped for subsequent cells, **unless every target is skipped**, in which case the filter does not apply. Skipped cells are already a distinct third outcome here — `compare.rs:63` states that "a skipped cell is NOT an errored one" — so the reporting slot exists.

**What it must NOT absorb.** Not the dollar ceiling: `crates\runner\src\budget.rs` owns spend, this owns time, and the two answer different questions ("can we afford it" vs "can we wait for it"). Not the responder's breaker (`crates\responder\src\breaker.rs`) — N breakers gating one call is a different shape from one filter over N candidates, and the responder's admission control is correct for the responder. Not the `MAX_TRIES` constant's configurability, which is a separate and smaller argument.

## The measurable

**Provider calls burned per matrix run against a degraded target: currently `cases` (60 in the reference matrix), target ≤ N (the filter's threshold).**

Measured by a new case beside the deterministic stand-in at `crates\runner\src\compare.rs:765`: a 10 × 60 matrix in which target 7 fails every call, asserting target 7's call count. The paired assertion is the degenerate case — all ten targets failing must still attempt every cell, never refuse — mirroring what `crates\responder\src\breaker.rs:56-57` already asserts for permits.

**Second number: attempts spent on a 429 carrying `Retry-After: 5`.** Currently three, spanning ~600ms, ending in failure. After: attempts spaced by the stated delay, and `errored_cases` on the run report (`compare.rs:641`) falling to near zero for a run against a merely-throttling provider. Instrument: the mock server's recorded request timestamps in the provider-boundary suite (`crates\engine\tests\provider_boundary.rs`, new; `wiremock` as a dev-dependency), which does not exist yet — `wiremock` / `mockito` / `httpmock` appear zero times in this tree — and which this direction depends on.

## What would make this wrong

**If providers tracklight benchmarks do not send the header.** The whole retry-after half rests on OpenAI, Anthropic and Gemini actually stating a delay on 429. If a week of `crates\engine` logs shows the header absent from every 429 this instance has ever received, the ladder is already doing the only thing available and this reduces to the deadline, which is smaller. That evidence is cheap to collect and should be collected first.

**If the matrix is never run against a degraded provider.** The filter's value is proportional to how often a benchmark meets an incident. If the run history shows `errored_cases` at zero across every recorded run, the filter is machinery guarding a case that has not occurred, and the honest sequencing is to land the retry-after half and defer the filter with this as its return condition.

**If the filter hides a real result.** A benchmark exists to measure providers, and "this provider was unavailable" is a measurement. A filter that silently skips a target produces a leaderboard with a missing row rather than a failing one. If the skipped cells cannot be rendered as distinctly as `partial` and `budget_halted` already are (`compare.rs:649-656`), the filter is trading the product's own output for latency and should not ship.
