# Judge calibration & the drift sentinel

An LLM-as-judge is only trustworthy if it agrees with humans. `lt-runner calibrate` measures that
agreement on a **human-labeled golden set** — Cohen's κ, Pearson, MAE/RMSE, and directional bias —
and declares the judge **trusted** when κ clears a bar. See `docs/BENCHMARK_FRAMEWORK.md` §3 for the
metrics.

But a one-shot number goes stale: a provider model update or a prompt tweak can silently erode
agreement, and you won't notice until benchmarks start looking weird. The **drift sentinel**
(`--watch`) re-runs calibration on a schedule, persists the κ history, and alerts when trust
degrades.

## One-shot calibration

```bash
lt-runner calibrate --file golden.jsonl --rubric "Answer is correct, complete, and grounded." \
  --threshold 0.7 --kappa-bar 0.6
# or a structured rubric:  --rubric-id <rubric-id>
# --samples N   self-consistency (judge each item N× and average, rubric mode)
# --report out.json   write the full metrics blob
```

`golden.jsonl` is one `{input, output, human_score, ...}` per line (or a JSON array). `human_score`
is the ground-truth quality in 0..1 the judge is measured against; calibration is **judge-only** — it
re-scores the given outputs, it does not generate. The judge engine is **unbudgeted**, so calibration
runs regardless of ingest limits.

## Watch mode (the drift sentinel)

```bash
# daemon: re-judge the golden set every hour
lt-runner calibrate --file golden.jsonl --rubric-id <id> --watch --interval 3600 --project <proj>

# single cycle for an external scheduler (cron / Cloud Scheduler); exits non-zero if untrusted
lt-runner calibrate --file golden.jsonl --rubric-id <id> --once --project <proj>
```

Each cycle:

1. reads the **previous** κ from the scores history (see the reserved rubric below),
2. re-judges the pinned golden set (up to `--jobs` concurrency; κ is identical at any `--jobs`),
3. **persists** this cycle's agreement as a score (below) — which also feeds the API's alerting,
4. computes a **drift verdict** and prints a compact per-cycle line; warnings go to **stderr**.

`--once` runs exactly one cycle and exits, so running "too often" from cron is harmless. It exits with
code **5** when the cycle ends *untrusted* (κ < `--kappa-bar`), so a scheduler/CI step can fail on a
degraded judge. Daemon mode always exits 0 and never dies on a transient cycle error (API briefly
down, one unparseable judge output) — it logs and continues, mirroring `lt-runner schedule`.

### Cron / external scheduler

Idempotency-by-nothing: unlike `schedule`, every watch cycle *does* record a fresh data point, so run
it on a real cadence (hourly/daily), not "as often as possible".

```cron
# daily at 03:00 — alert (non-zero exit) if the judge fell below the trust bar
0 3 * * * LIGHTTRACK_URL=http://127.0.0.1:8787 LIGHTTRACK_KEY=lt_... \
  /usr/local/bin/lt-runner calibrate --once --project myproj \
  --file /etc/lighttrack/golden.jsonl --rubric-id my-rubric \
  >> /var/log/lighttrack-calibrate.log 2>&1 || echo "judge untrusted" | mail -s drift oncall@x
```

## Persistence: the reserved rubric (no new table)

History is stored through the **existing** `POST /v1/scores` — no new table, no schema change. Each
cycle posts one [`Score`] under a **reserved rubric name**:

```
lt:calibration:<provider>/<model>        e.g.  lt:calibration:anthropic/haiku
```

| Score field | Carries |
|-------------|---------|
| `rubric`    | the reserved name above (per judge model) |
| `value`     | Cohen's **κ** for this cycle |
| `max`       | `1.0` |
| `pass`      | **trusted** (κ ≥ `--kappa-bar`) |
| `reasoning` | a compact JSON blob of the full metrics (κ, pearson, mae, rmse, bias, rates, n, threshold, kappa_bar, trusted, judge_cost_usd) |
| `scored_by` | `<provider>/<model>` of the judge |

### Querying the history

Read it back with the existing endpoint and filter by the reserved rubric client-side (the scores
list has no server-side rubric filter):

```bash
curl -s "$LIGHTTRACK_URL/v1/scores?project=myproj&limit=500" \
  | jq '[.[] | select(.rubric=="lt:calibration:anthropic/haiku")]
        | map({t:.created_at, kappa:.value, trusted:.pass})'
```

Scores come back **newest-first**, so the first match is the latest cycle — which is exactly how the
sentinel reads the previous run's κ for drift detection. To plot κ over time, sort the results by
`created_at`.

## Alerting: riding the existing `score_drop` channel

The sentinel builds **no parallel alert channel**. Two complementary signals cover drift:

1. **Immediate, per-cycle (runner-side).** Right after a cycle the runner compares this κ to the
   previous run's κ:
   - κ **below the bar** → `ALERT untrusted` on stderr, and `--once` exits `5`.
   - κ still above the bar but **dropped by more than `--drift-threshold`** (default `0.15`) vs the
     previous run → `WARN drift` on stderr (an early warning before it crosses the bar).

   This fires on the **very next** bad run — no warm-up window needed.

2. **Server-side, over the window (the existing alert machinery).** Every `POST /v1/scores` feeds the
   API's rolling `score_drop` detector, keyed by `(project, rubric)`. Because calibration κ is posted
   as scores under the reserved rubric, a **degrading κ trend rides the configured alert channels
   automatically** (webhook / ntfy / email) — the same path that catches a quality regression on any
   rubric. No calibration-specific wiring.

Configure the server-side channel on the **API** (see `docs/ALERTS.md`); the relevant knobs:

| Env (on the API) | Meaning | Default |
|------------------|---------|---------|
| `LIGHTTRACK_ALERT_WEBHOOK` / `LIGHTTRACK_ALERT_NTFY` / `LIGHTTRACK_ALERT_RESEND_KEY` | delivery channels | — |
| `LIGHTTRACK_ALERT_SCORE_WINDOW` | rolling per-(project,rubric) score window | `20` |
| `LIGHTTRACK_ALERT_SCORE_MIN_SAMPLES` | min cycles before a regression can trip | `8` |
| `LIGHTTRACK_ALERT_SCORE_DROP` | recent-vs-baseline mean drop that trips `score_drop` | `0.15` |

> Because the server-side detector needs `LIGHTTRACK_ALERT_SCORE_MIN_SAMPLES` cycles to warm up, the
> runner's immediate per-cycle check is what gives you a same-run signal; the server-side channel is
> what turns a sustained slide into a delivered webhook/email once enough history exists.

## Flags (watch mode)

| Flag | Meaning | Default |
|------|---------|---------|
| `--watch` | run the sentinel as a daemon | off |
| `--once` | run a single cycle and exit (implies watch); non-zero exit if untrusted | off |
| `--interval <secs>` | seconds between daemon cycles | `3600` |
| `--kappa-bar <κ>` | trust bar; κ below it ⇒ untrusted | `0.6` |
| `--drift-threshold <Δ>` | warn when κ drops more than Δ vs the previous run | `0.15` |
| `--project <id>` | project to attach scores to / scope the history read (else derived from the API key) | — |
| `--threshold <t>` | pass/fail cutoff for binarizing scores (drives κ) | `0.7` |
| `--samples <n>` | judge each item n× and average (rubric mode) | `1` |
