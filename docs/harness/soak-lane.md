# The store soak lane

*Declared 2026-08-24. First green: 2026-08-24.*

The long lane: sustained ingest against the embedded store, under read load, with the quiet-window
maintenance pass running alongside, judged against criteria that were written down before the runs
that judge them.

- **Criteria:** [`soak-criteria.json`](./soak-criteria.json) — the single authority. The harness reads
  it with `include_str!`, so a missing or renamed file is a build error rather than a lane that
  quietly certifies nothing.
- **Harness:** `crates/store/tests/soak.rs`
- **Clock:** `.github/workflows/soak.yml` — nightly at 03:20 UTC, plus `workflow_dispatch` with a
  duration input.

## Why it is not a gate

It answers questions only time and pressure can ask — does write latency hold its *shape* over a
sustained run, does the file grow in proportion to the data, does the journal sidecar stay bounded —
and none of those is a property of any single change. Blocking a merge on a run like this would
destroy the merge cadence without improving the certification, so the lane runs on its own clock and
is never a required check. It also stays out of `ci.yml` for a second reason: branch protection is
configured from that file's job names, and `crates/core/tests/gate_table_guard.rs` requires every job
there to appear in CONTRIBUTING's gate table. A lane that is not a gate has no business in the gate
table.

## Two modes

| mode | when | duration | enforces |
| --- | --- | --- | --- |
| harness liveness | every `cargo test --workspace` | ~8 s (two 4 s phases) | that the lane measures, writes an artifact, and **fires on the planted defect** |
| certification | nightly / dispatch, `LIGHTTRACK_SOAK_ENFORCE=1` | 2 × 300 s by default | every criterion in the criteria file |

The short mode is deliberately **not** a skip. A lane that is wired in and never exercised is the
failure mode that hides in plain sight: it fails from its first day, red becomes normal, and every
failure after the first is wallpaper. A lane with a 100% historical failure rate is not a flaky lane,
it is an **unbuilt lane wearing a gate's clothes**, and the finding it reports is about the harness
rather than the product. Running the harness on every change is the cheapest possible guard against
that — the lane cannot silently stop existing, because the workspace suite would go red.

What the short mode does *not* do is enforce the timing bounds, because a shared runner's noise would
turn a blocking gate into a coin flip. The bounds are the nightly's verdict.

## Lane health: earned green, planted red

Both are asserted on **every** run, in both modes:

- **Earned green** — the known-good configuration must satisfy every criterion.
- **Planted red** — a second run injects a deliberate, growing latency into the write path through
  its second half, and the lane must fail it *on `write_p95_drift_ratio` specifically*. A lane that
  has never been observed to fail for cause is indistinguishable from a lane that cannot fail; a lane
  that fails for the wrong reason certifies nothing at all.

The planted-red assertion earned its place immediately. The first implementation applied the injected
sleep *after* the latency measurement, so it slowed the run's throughput and moved the measured
latency not at all — the assertion caught it on the first execution. A second version injected from
the first bucket of the second half onward, which also inflated the trend criterion's *denominator*
and left the ratio marginal enough to go quiet under ambient machine load; the injection now starts
one bucket later, so the denominator is an honest baseline.

## Reading a run

Every run emits one artifact — the criteria, the measured series, the verdict, and the lane-health
block — and the nightly uploads it with 90-day retention. **The sequence of artifacts is the lane's
dashboard.** A single green verdict is the weakest thing the lane produces: a regression that stays
inside its bound is still a regression, and only the trend line catches it. The workflow's step
summary renders the same numbers inline, including on a failed run.

Every figure carries its predicate, in the artifact, beside the number:

- `write_p95_ms` — a percentile, never an average. An average hides exactly the tail the lane exists
  to see.
- `write_p95_drift_ratio` — the slope measured **within** the run's second half (final bucket ÷ the
  second half's first bucket). A p95 that sits under its ceiling at the finish is still compatible
  with linear degradation that clears the ceiling an hour later, so the slope is judged separately
  from the endpoint. Measured inside the second half so the opening bucket's cache warm-up cannot
  masquerade as degradation.
- `db_bytes_per_event` — the resource criterion. The file is *supposed* to grow; growing faster than
  the data is the finding.
- `wal_bytes_max` — the criterion the maintenance pass exists to hold. Retention is deliberately
  unbounded (operator, 2026-08-24 — `docs/ARCHITECTURE.md` §12), which makes the journal the one
  growth axis engineering still controls.

## Workload reality

A load lane certifies only the traffic it generates, so the workload's shape travels with the verdict
in the artifact rather than being implied by it. The shape here is declared **approximate**: this
instance has no fleet telemetry to derive a real operation mix from, and the criteria file says so in
`workload.shape_note` instead of presenting a guess as a profile. One writer is not an approximation
but a fact — the SQLite backend serialises writes behind a single connection by design, so a second
writer would measure queueing rather than the store. When a real traffic profile exists, it replaces
that block and the bounds are re-declared against it.

## Changing a bound

Edit `soak-criteria.json`, and edit `how_the_bounds_were_calibrated` in the same change to say what
was measured. Criteria adjusted while looking at a run's results are not criteria; they are
commentary. The bounds as declared sit roughly 2.5–7× above a measured baseline (a 30 s enforced run
on 2026-08-24: 10 621 events, 0 errors, write p95 8.1 ms, drift 1.25×, 2 494 bytes/event, peak
journal 18.7 MiB) — headroom for a *different machine*, not headroom for a regression.

## If the lane goes flaky

Quarantine, never delete: a flaky check is the only instrument pointed at whatever is intermittently
wrong, and deleting it removes the instrument while keeping the problem. A quarantined criterion is
one moved out of the enforced set **with its owner, its entry date and its failure signature written
down here**, still measured and still reported in the artifact, and reviewed on a schedule with two
exits — fixed and restored, or a recorded decision that the claim is not worth carrying. Quarantine
without scheduled review is deletion with a waiting period.

*Quarantined criteria: none.*

## Relationship to `crates/store/src/sqlite/bench.rs`

That harness stays, and it is a different instrument: a one-off A/B comparison of store *shapes*
(rollback journal vs WAL, pooled reads vs single mutex), hand-run when the concurrency model changes.
It measures rather than judges, has no declared bounds and no artifact — which is exactly why it was
never a lane, and why it is not deleted now that one exists.
