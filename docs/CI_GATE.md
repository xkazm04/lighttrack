# Benchmarks as a CI gate

A LightTrack benchmark can gate a build: fail CI when the judged quality of a prompt/model
regresses against its recorded `baseline_score`. Two surfaces make this machine-consumable — an
exit-code contract on the runner, and a JSON gate endpoint on the API — plus an optional
completion webhook.

## Verdict vocabulary

Every finished run carries one honest status (see `BENCHMARK_FRAMEWORK.md`):

| status        | meaning                                                                 |
|---------------|-------------------------------------------------------------------------|
| `passed`      | a baseline exists and the run held (significance-aware; see below)      |
| `regressed`   | the run's ~95% confidence interval on the mean sits **below** baseline  |
| `no_baseline` | the benchmark has no `baseline_score` to gate against                   |

"Regressed" is **significance-aware**: with `n ≥ 2` cases the whole `mean ± 1.96·stderr` interval
must fall below the baseline, so a noisy 3-case run does not trip as easily as a 3000-case run. With
`n < 2` there is no stderr, so it falls back to a plain scalar compare and annotates the run report
(`caveats: ["scalar fallback, n=1"]`).

## 1. Runner exit codes — `lt-runner bench --gate`

Without `--gate`, `lt-runner bench` exits `0` on success as before. With `--gate` it maps the
run-level verdict to a process exit code:

| exit code | verdict       |
|-----------|---------------|
| `0`       | `passed`      |
| `3`       | `regressed`   |
| `4`       | `no_baseline` |

`no_baseline` gets its own code so CI can treat "unverified" differently from a real regression
(e.g. warn instead of hard-fail). In compare mode the exit code reflects the **aggregate** verdict
across all targets (`regressed` if any target regressed).

```bash
lt-runner bench --benchmark "$BENCHMARK_ID" --gate
```

## 2. Gate endpoint — `GET /v1/benchmarks/:id/gate`

Returns the verdict of the **latest finished run**, without re-running anything:

```json
{ "status": "regressed", "run_id": "…", "mean": 0.5, "baseline": 0.8, "n": 30 }
```

`status` is one of `pass | regressed | no_baseline | no_runs` (`no_runs` when no finished run
exists yet). Useful for a dashboard badge or a pipeline step that reads the last recorded run
instead of running the benchmark itself.

## 3. Completion webhook (optional)

Set `LIGHTTRACK_BENCH_WEBHOOK` (falls back to `LIGHTTRACK_ALERT_WEBHOOK`) and the API POSTs each
finished run, best-effort and off the request path, deduped per `(benchmark, status)` within the
alert cooldown:

```json
{ "event": "bench_run", "benchmark": "…", "run_id": "…", "status": "regressed",
  "mean": 0.5, "baseline": 0.8, "text": "LightTrack benchmark … finished: regressed (mean 0.500 vs baseline 0.800)" }
```

## GitHub Actions example

```yaml
name: quality-gate
on: [pull_request]
jobs:
  benchmark:
    runs-on: ubuntu-latest
    env:
      LIGHTTRACK_URL: ${{ secrets.LIGHTTRACK_URL }}
      LIGHTTRACK_KEY: ${{ secrets.LIGHTTRACK_KEY }}
    steps:
      - uses: actions/checkout@v4
      # Build or download lt-runner, then gate on the benchmark.
      - name: Run benchmark as a gate
        run: |
          lt-runner bench --benchmark "${{ vars.BENCHMARK_ID }}" --gate
          # exit 0 = passed, 3 = regressed (fails the job), 4 = no baseline.

      # Alternatively, gate on the latest recorded run without re-running the benchmark:
      - name: Check latest gate verdict
        run: |
          status=$(curl -fsS -H "authorization: Bearer $LIGHTTRACK_KEY" \
            "$LIGHTTRACK_URL/v1/benchmarks/${{ vars.BENCHMARK_ID }}/gate" | jq -r .status)
          echo "gate status: $status"
          [ "$status" = "regressed" ] && { echo "::error::benchmark regressed"; exit 1; } || true
```

To treat "no baseline" as a soft failure while still hard-failing on a regression, branch on the
exit code:

```bash
lt-runner bench --benchmark "$BENCHMARK_ID" --gate
code=$?
case $code in
  0) echo "passed" ;;
  3) echo "::error::regressed"; exit 1 ;;
  4) echo "::warning::no baseline — not gated"; exit 0 ;;
  *) echo "::error::runner error ($code)"; exit $code ;;
esac
```
