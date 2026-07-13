# Scheduled online sampling

`lt-runner schedule` periodically samples recent live events for a project, scrubs PII, and freezes a
new **dataset** — so your evaluation data keeps tracking real traffic instead of going stale. Pair the
resulting datasets with `lt-runner bench` / `calibrate` to benchmark against fresh, representative data.

It runs either as a **daemon** (its own interval loop) or as a **single cycle** (`--once`) driven by
an external scheduler (OS cron, a systemd timer, Windows Task Scheduler, Cloud Scheduler).

## How it samples

Each cycle:
1. fetches the most recent `--n` events for the project,
2. names the dataset `"<prefix>-<id8>"` after the **newest event that carries an input** (the
   "watermark"),
3. **skips** if a dataset for that watermark already exists, or if there's nothing with an input to
   sample,
4. otherwise scrubs PII (regex always; optional `--llm-scrub` pass) and freezes the dataset.

Because the name is derived from the data (not the wall clock), the cycle is **idempotent**: idle
periods cost nothing and never produce duplicate snapshots — even across separate `--once` runs. New
traffic advances the watermark, which produces the next dataset.

> The judge/scoring engine is unbudgeted; `--llm-scrub` makes one `claude -p` call per item, so it has
> a cost. Plain regex scrubbing is free. See `docs/DECISIONS.md` D9.

## Daemon mode

```bash
export LIGHTTRACK_URL=http://127.0.0.1:8787
export LIGHTTRACK_KEY=lt_...          # a project key (or set in enforced mode); dev mode needs none
lt-runner schedule --project <id> --interval 3600 --n 50
# --interval seconds between cycles · --n events per cycle · --name-prefix <p> · --llm-scrub
```

## External schedulers (use `--once`)

Each invocation runs exactly one cycle and exits; idempotency means running "too often" is harmless.

**Linux cron** — hourly:
```cron
0 * * * * LIGHTTRACK_URL=http://127.0.0.1:8787 LIGHTTRACK_KEY=lt_... \
  /usr/local/bin/lt-runner schedule --once --project myproj --n 50 >> /var/log/lighttrack-sample.log 2>&1
```

**systemd timer** — `lighttrack-sample.service` + `lighttrack-sample.timer`:
```ini
# lighttrack-sample.service
[Service]
Type=oneshot
Environment=LIGHTTRACK_URL=http://127.0.0.1:8787
Environment=LIGHTTRACK_KEY=lt_...
ExecStart=/usr/local/bin/lt-runner schedule --once --project myproj --n 50

# lighttrack-sample.timer
[Timer]
OnCalendar=hourly
Persistent=true
[Install]
WantedBy=timers.target
```

**Windows Task Scheduler** — hourly:
```powershell
$env:LIGHTTRACK_URL = "http://127.0.0.1:8787"
schtasks /Create /TN "LightTrack sample" /SC HOURLY `
  /TR "C:\path\lt-runner.exe schedule --once --project myproj --n 50"
```

**GCP Cloud Scheduler** (Phase 5) — trigger a Cloud Run **job** running the same `schedule --once`
command on a cron schedule; the runner reads `LIGHTTRACK_URL`/`LIGHTTRACK_KEY` from the environment /
Secret Manager.

---

# Auto-scoring traces (`score-traces`)

`lt-runner score-traces` makes whole traces **score themselves**: on a schedule it samples recently
**completed** traces for a project, judges each one's **root exchange** with an LLM judge, and posts a
whole-trace score — so nobody has to run `POST /v1/traces/:id/score` by hand at 2am. Like `schedule`,
it runs as a **daemon** (`--interval`) or a **single cycle** (`--once`) under an external scheduler.

```bash
export LIGHTTRACK_URL=http://127.0.0.1:8787
export LIGHTTRACK_KEY=lt_...   # project key (dev mode needs none)
# Judge every settled trace's root exchange against freeform criteria, once (for cron):
lt-runner score-traces --project <id> --rubric "Did the assistant fully answer the user?" --once
# Or judge against a structured rubric, sampling 1 in 5 traces but always judging errors, as a daemon:
lt-runner score-traces --project <id> --rubric-id <rid> --sample-every 5 --errors-always --interval 3600
```

## What one cycle does

1. Computes a **settle cutoff** = `now − --settle-secs` (default **120s**) and walks the project's
   traces whose newest event is older than it — newest-ended first — through the `/v1/traces` keyset
   window (`until=<cutoff>`, following `X-Next-Cursor`), up to `--limit` traces (default 100).
2. Takes a **stable 1/N sample** (`--sample-every N`, default 1 = all): a trace is in the sample iff a
   hash of its `trace_id` falls in the 1/N bucket — order-independent, so the same subset is picked
   every cycle. With `--errors-always`, **every** error trace is judged on top of the sample.
3. For each sampled trace, fetches the trace detail and **skips** it if it already has a whole-trace
   score for this rubric (idempotency, see below) or if its root span carries **no output** to judge.
4. Judges the surviving traces' root exchanges (root span's `input`/`output`) with the judge model —
   concurrently, bounded by the global `--jobs` — and posts each verdict to `POST /v1/traces/:id/score`
   with no `event_id`, so the score anchors to the trace's root span (the whole-request judgment).

`--rubric "<criteria>"` uses a freeform judge (the criteria text is the score's `rubric` label);
`--rubric-id <id>` fetches a structured rubric and judges per-dimension (its `name` is the label).
Pass exactly one. `--judge "[provider/]model"` overrides the judge model for this run.

## "Completed" is a settle window

Traces carry **no explicit completion marker** — new spans can always arrive later. So a trace is
*approximated* as completed once its newest event (`ended`) is older than the settle window
(`--settle-secs`, default 120s). Choose it larger than your longest in-flight request so a
still-streaming trace isn't judged mid-flight, and smaller if you want fresher scoring; there is no
correctness cost either way, only how soon a trace becomes eligible.

## Idempotent — never double-scores

Before judging, `score-traces` checks the trace's existing scores: if one already has this run's
`rubric` label anchored to the root event, the trace is skipped. Because the score it posts anchors to
that same root, the very next cycle sees it and skips — so a daemon **and** repeated cron `--once` runs
converge to "each trace scored once per rubric", never twice. Changing the rubric text / id scores the
traces afresh under the new label. The judge is **unbudgeted** (it never counts against ingest limits)
and uses only existing API endpoints.

## External schedulers (use `--once`)

Identical shape to `schedule --once` above — one cycle per invocation, safe to run "too often". e.g.
**Linux cron**, every 15 min:
```cron
*/15 * * * * LIGHTTRACK_URL=http://127.0.0.1:8787 LIGHTTRACK_KEY=lt_... \
  /usr/local/bin/lt-runner score-traces --once --project myproj \
  --rubric "Did the assistant fully answer the user?" --errors-always >> /var/log/lighttrack-score.log 2>&1
```
A one-shot run **exits non-zero** if the cycle fails (e.g. the API is unreachable), so a scheduler step
surfaces the failure; a daemon logs and continues instead.
