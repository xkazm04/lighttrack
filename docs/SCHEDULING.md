# Scheduling — recurring work is a row, not a running process

Everything LightTrack does on its own — re-running a benchmark, judging new events, scoring traces,
sampling live traffic into a frozen dataset, re-measuring the judge against a golden set — is one
mechanism: a **job** of some **kind**, and optionally a stored **schedule** that enqueues it on an
interval.

That is a change. Until M7 the queue carried exactly one kind (`bench_run`), the other four
workloads each shipped as a separately-scheduled daemon (`lt-runner score --interval`,
`score-traces --interval`, `schedule --interval`, `calibrate --watch`), and benchmark recurrence was
a key smuggled into a benchmark's `target` JSON. Three consequences, all of which the current design
exists to remove:

- everything the queue provides — a lease with a heartbeat, cancellation, honest retry accounting,
  live progress, a durable record that the work ran — protected **one of five** workloads;
- **compare benchmarks could not recur at all**: a matrix `target` is an array, and an array has no
  room for a `schedule_interval_secs` key;
- nothing could answer *"what runs on a schedule on this instance?"* — the answer lived in five
  daemons' command lines.

## The five job kinds

| Kind | One cycle of | Required payload | Notable optional fields |
|---|---|---|---|
| `bench_run` | a stored benchmark | `benchmark_id` | `samples`, `gen_samples`, `heal`, `pairwise`, `jobs` |
| `score_events` | judging recent unscored events | `rubric` **or** `rubric_id` | `project`, `limit` |
| `score_traces` | judging whole traces | `project` + `rubric`/`rubric_id` | `sample_every`, `errors_always`, `settle_secs`, `limit`, `judge_model` |
| `dataset_sample` | sampling live events into a frozen dataset | `project` | `n`, `name_prefix`, `llm_scrub` |
| `calibrate` | judge/human agreement against a golden set | `file` + `rubric`/`rubric_id` | `project`, `threshold`, `kappa_bar`, `drift_threshold`, `samples` |

A payload is validated **against its kind at the door** (`POST /v1/jobs`, and when a schedule is
written). A malformed one is a `400` there rather than a job that claims, runs, fails, retries twice
and dead-letters — three claims and a dead job to report a typo.

> `calibrate`'s `file` is a path **on the worker's filesystem**. Calibration items are the operator's
> own labelled data and deliberately never transit the API.

## Enqueue one unit of work

```bash
curl -sX POST "$LIGHTTRACK_URL/v1/jobs" -H "authorization: Bearer $ADMIN_KEY" \
  -H 'content-type: application/json' \
  -d '{ "type": "score_traces", "payload": { "project": "proj-a", "rubric_id": "r-1" } }'
```

```bash
lt jobs enqueue --type bench_run --payload '{"benchmark_id":"b-1","samples":2}'
lt jobs list --status running
lt jobs show <job-id>
lt jobs cancel <job-id>
```

## Make it recur

```bash
lt schedules create --project proj-a --type bench_run --every 6h \
  --payload '{"benchmark_id":"b-1"}'

lt schedules list                 # every recurring workload in the deployment
lt schedules set <id> --disable   # pause it; it stays listed
lt schedules runs <id>            # the jobs it has produced
```

A schedule is `{ project_id, kind, payload, interval_secs, next_due, last_job_id, enabled }`. Routes:

| Route | Who | What |
|---|---|---|
| `POST /v1/projects/:id/schedules` | admin | create (payload validated against the kind) |
| `GET /v1/projects/:id/schedules` | project key or admin | one project's schedules |
| `GET /v1/schedules` | admin | every schedule in the deployment |
| `PUT /v1/schedules/:id` | admin | patch — omitted fields are left alone |
| `DELETE /v1/schedules/:id` | admin | remove (the jobs it produced are kept) |
| `GET /v1/schedules/:id/runs` | admin | the jobs it has produced |

### Who sweeps

**The API process**, on a timer (`LIGHTTRACK_SCHEDULE_SWEEP_SECS`, default 60s, floor 10, `0` = off).
Not the runner: a Cloud Run deployment ships the API alone, so recurrence hosted in the optional
companion worker would silently not happen in the deployment that most needs it. The same sweep also
reaps dead relay leases — see `docs/RELAY.md`.

On by default, unlike the forecast sweep. That one turns a self-hosted instance into an outbound
notifier, which is a decision; this one is upkeep of schedules the operator wrote down themselves,
and a stored schedule that does not fire is just a broken feature.

Two rules the sweep keeps:

- **Idempotent.** A schedule whose previous job is still queued or running is skipped, so a benchmark
  that takes longer than its own interval never stacks a second copy of itself.
- **No catch-up.** `next_due` advances from *now*, not from the old due time. A sweep that was down
  for a day comes back and fires each schedule once, not a day's worth of intervals at once — which
  for a benchmark would be a day of generation spend nobody asked for.

### Migration from `target.schedule_interval_secs`

On boot, every benchmark still carrying the old recurrence key gets an equivalent `bench_run`
schedule if it does not already have one (idempotent, so it is safe on every boot). The key stays
readable for one release; nothing deletes it. Recurrence you configured keeps working, and a
**compare** benchmark can now recur, which it never could before.

## The worker

```bash
lt-runner serve                                   # claims every kind
lt-runner serve --kinds bench_run,score_traces    # …or only what this machine can run
```

`--kinds` is a capability declaration, not a convenience filter: the API applies it **inside the
atomic claim**. A worker that claims a kind it cannot execute has already taken the job off the queue
and stamped a lease on it, so the job burns its retry budget failing while a capable worker sits idle
beside it. `--providers` defaults to the providers whose API keys are present in the worker's
environment.

Each daemon subcommand also takes `--via-queue`, which enqueues one cycle and serves it here — the
same work, with the queue's lease, cancellation and record around it:

```bash
lt-runner score --rubric-id r-1 --via-queue
```

## Externally-driven scheduling

If you would rather own the cadence (OS cron, systemd timers, Cloud Scheduler), skip schedules
entirely and post jobs:

```cron
0 * * * *  curl -sX POST "$LIGHTTRACK_URL/v1/jobs" -H "authorization: Bearer $ADMIN_KEY" \
             -H 'content-type: application/json' \
             -d '{"type":"dataset_sample","payload":{"project":"myproj","n":50}}'
```

`GET /v1/capabilities` says whether this deployment's store backend serves the `schedules` surface at
all; where it does not, those routes answer `501 unsupported` and this is the supported path.

## Online sampling (`dataset_sample`) in detail

Each cycle:

1. fetches the most recent `n` events for the project,
2. names the dataset `"<name_prefix>-<id8>"` after the **newest event that carries an input** (the
   "watermark"),
3. **skips** if a dataset for that watermark already exists, or there is nothing with an input,
4. otherwise scrubs PII (regex always; optional `llm_scrub` pass) and freezes the dataset.

Because the name is derived from the data and not the wall clock, the cycle is idempotent: idle
periods cost nothing and never produce duplicate snapshots. New traffic advances the watermark, which
produces the next dataset. A skipped cycle is a **successful** job — that is the mechanism working,
not a failure to retry.

> The judge/scoring engine is unbudgeted; `llm_scrub` makes one `claude -p` call per item, so it has
> a cost. Plain regex scrubbing is free. See `docs/DECISIONS.md` D9.

Pair the resulting frozen datasets with `bench_run` / `calibrate` schedules so your evaluation data
keeps tracking real traffic instead of going stale.

## The direct commands still work

Nothing was removed. `lt-runner score`, `score-traces`, `schedule` and `calibrate --watch` run
exactly as before, in-process, with their own `--interval`. They are the right tool for a one-off or
a debugging session. What they cannot give you is a record: a daemon nobody restarted is
indistinguishable from a schedule nobody created, and only one of those is visible in
`GET /v1/schedules`.
