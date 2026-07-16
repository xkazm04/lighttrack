# Performance Optimizer — Alert Attribution & Channels

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. Alert channels fan out strictly serially — one slow sink stalls every other channel and every other breach

- **Severity**: High
- **Category**: serial-fanout
- **File**: `crates/api/src/alerts/channels.rs:51-69` (also `75-83`, `85-91`, `93-113`, `127-129`, `143-145`)
- **Scenario**: A project trips 3 caps at once (cost/calls/tokens on the same window) with all three sinks configured (webhook + ntfy + Resend). Each `deliver_*` awaits `post_webhook` → `post_ntfy` → `post_resend` in sequence, inside a `for b in breaches` loop that is itself serial.
- **Root cause**: Every POST is `.await`ed to completion before the next one starts. The three channels are fully independent (no shared state, no ordering requirement, each is a no-op when unconfigured), and breaches are independent of one another. The `reqwest::Client` timeout is 5s (`alerts.rs:130`), so the worst case per breach is 3 × 5s = 15s, and for 3 breaches 45s — all of it wall-clock in the spawned delivery task.
- **Impact**: Latency is `sum` where it should be `max`. Healthy sinks (~200ms each) drop from ~600ms to ~200ms per breach; the pathological case (ntfy hung, hitting the 5s timeout) drops from 45s → ~5s for a 3-breach burst. Worse than the latency: while the task is parked on a dead ntfy, the *webhook and email for the other breaches haven't been sent yet* — a down secondary sink delays the primary alert. A 5s+ delay on a "you are burning money right now" alert is the whole point of the feature.
- **Fix sketch**: Inside each `deliver_*`, replace the three sequential awaits with `tokio::join!(post_webhook(..), post_ntfy(..), post_resend(..))` — all three futures are independent and each already swallows its own errors, so `join!` needs no error plumbing. Then fan the outer loop with `futures::stream::iter(breaches).for_each_concurrent(4, |b| async move { … })` (a small cap, not unbounded, so a 50-breach burst doesn't hammer a receiver's rate limit). `deliver_breaches` needs `msg`/`contributors` computed per-item inside the closure — they already are.
- **Trade-offs**: Ordering of delivery across breaches becomes non-deterministic; message *content* is unaffected, and receivers already can't rely on ordering across the spawned task. Concurrent posts to the same webhook may arrive out of order — acceptable for alerts (each carries its own `breach` payload), but if a Slack receiver renders a thread, cap `for_each_concurrent` at 1 for the outer loop and keep `join!` on the inner three (that alone captures most of the win).

## 2. Attribution re-runs the same two window-wide aggregate scans once per breach

- **Severity**: High
- **Category**: hot-path
- **File**: `crates/api/src/alerts/attribution.rs:69-82` (driven by the per-breach loop at `crates/api/src/alerts.rs:216-227`)
- **Scenario**: A project with monthly cost + calls + tokens caps trips all three in the same evaluation. `attribute()` loops over the breaches and calls `fetch` for each; every call issues `cost_summary_windowed(project, since)` and `usecase_costs(project, since)`. All three breaches share the same `project_id` and the same `LimitWindow`, so all three compute the identical `since` and re-run the identical pair of queries — 6 aggregate scans where 2 would do.
- **Root cause**: `fetch` is keyed on the individual `LimitStatus`, not on the `(project, window)` pair that actually determines the rollup. The scope is applied *after* fetching, purely in `compose`, so the fetched rows are already scope-independent — the duplication is not doing any distinguishing work. Each query is a `GROUP BY project_id, provider, model` (`store/src/sqlite/events.rs:499-501`) / `GROUP BY name, provider, model` over every event in the window; the `(project_id, ts)` index serves the range, but the rows in range are all read and grouped.
- **Impact**: Query work scales as `O(breaches × events_in_window)` instead of `O(distinct_windows × events_in_window)`. On a monthly cap over a busy project (~2M events), each pair of scans is on the order of hundreds of ms to seconds of blocking-pool time and reads the same pages three times. Deduplicating cuts it to 1/N for the multi-cap case (the common one — projects rarely configure exactly one rule).
- **Fix sketch**: In `alerts.rs::attribute`, build a `HashMap<(String, LimitWindow), (Vec<CostRow>, Vec<UseCaseCostRow>)>` rollup cache in the `spawn_blocking` closure: for each breach, fetch-or-insert on `(project_id, window)`, then call the already-pure `attribution::compose(&cost_rows, &usecase_rows, b.scope.as_ref())` per breach. This needs a small split of `fetch` into `fetch_rows(store, project, window, now) -> (Vec<CostRow>, Vec<UseCaseCostRow>)` + the existing `compose`, which the module is already structured for (the I/O-vs-pure split is the file's stated design). `LimitWindow` needs `Hash`/`Eq` — or key on the `since` timestamp.
- **Trade-offs**: None material. The cache is per-invocation (a few rows, dropped at the end of the task), so no staleness question; all breaches in one delivery already share a single `now` (`alerts.rs:214`), so they're already expected to agree on the window.

## 3. Scoped model/use-case breaches fetch a full cost rollup that `compose` never reads

- **Severity**: Medium
- **Category**: hot-path
- **File**: `crates/api/src/alerts/attribution.rs:77-81` (consumers at `104-120`)
- **Scenario**: An operator caps a specific model (`LimitScope::Model("gpt-4o")`) or a use-case (`LimitScope::Name("summarize")`) — the two most natural scoped caps. On breach, `fetch` unconditionally runs `cost_summary_windowed` *and* `usecase_costs`, but the `Model` and `Name` arms of `compose` read only `usecase_rows`; `cost_rows` is dropped unused. The unit tests make this explicit — both pass `&[]` for `cost_rows` and still produce full attribution (`attribution.rs:247`, `261`).
- **Root cause**: `fetch` eagerly gathers both rollups before dispatching on scope, so the query set doesn't reflect what the scope actually consumes.
- **Impact**: Exactly one wasted window-wide `GROUP BY` scan over `events` per scoped model/name breach — roughly 50% of attribution's I/O for those cases, since the two queries scan the same rows with the same predicate and differ only in grouping. On a monthly window over a large project that's a multi-hundred-ms blocking-pool scan producing a value that is immediately discarded. Compounds multiplicatively with finding #2 (3 scoped breaches → 3 wasted scans).
- **Fix sketch**: Gate the fetch on scope in `fetch`: `let needs_costs = matches!(scope, None | Some(LimitScope::Provider(_)));` and only call `cost_summary_windowed` when true, passing `&[]` otherwise. `usecase_rows` is needed by every arm, so it stays unconditional. If #2's `fetch_rows` split lands first, the cache key becomes `(project, window, needs_costs)` — or simpler, keep fetching costs whenever *any* breach in the batch needs them.
- **Trade-offs**: Couples `fetch`'s query set to `compose`'s match arms — a future arm that starts reading `cost_rows` would silently see an empty slice. Mitigate by putting the `needs_costs` predicate next to `compose` (or on `LimitScope` itself) and asserting the mapping in a test, rather than leaving the two matches to drift apart in separate functions.
