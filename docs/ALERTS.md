# Breach alerts

When an ingested event trips a project limit (cost / calls / tokens × hour / day / month), the API
logs an `[ALERT]` line and — if configured — delivers the breach to a **webhook** and/or **ntfy**
endpoint.

Delivery is **best-effort** and **off the request path** (a spawned task), so a slow or down sink
never delays or fails ingest. Alerts are **deduplicated** per `(project, metric, window)` with a
cooldown, so a sustained breach (which trips on every ingest until the rolling window clears) doesn't
spam the channel.

## Configure (env on the API)

Config is server-global (env). `alerts.rs::from_env` is the source of truth for these keys.

**Channels** (set any combination; at least one enables alerting):

| Env | Meaning |
|-----|---------|
| `LIGHTTRACK_ALERT_WEBHOOK` | POST a JSON body to this URL (Slack/Discord/custom) |
| `LIGHTTRACK_ALERT_NTFY` | POST a text body to this ntfy topic URL |
| `LIGHTTRACK_ALERT_RESEND_KEY` | Resend API key — enables **email** delivery |
| `LIGHTTRACK_ALERT_EMAIL_TO` | comma-separated recipient(s); **required** for email |
| `LIGHTTRACK_ALERT_EMAIL_FROM` | sender (default `onboarding@resend.dev`, Resend's shared test sender; a real domain must be verified in Resend) |
| `LIGHTTRACK_BENCH_WEBHOOK` | dedicated benchmark-completion webhook; falls back to `LIGHTTRACK_ALERT_WEBHOOK` |

**Tuning:**

| Env | Meaning | Default |
|-----|---------|---------|
| `LIGHTTRACK_ALERT_COOLDOWN_SECS` | re-alert window per dedup key | `3600` |
| `LIGHTTRACK_ALERT_ERROR_THRESHOLD` | failed calls per window that trip an error-spike | `5` |
| `LIGHTTRACK_ALERT_ERROR_WINDOW_SECS` | rolling window for the error-spike counter | `300` |
| `LIGHTTRACK_ALERT_SCORE_WINDOW` | per-(project,rubric) score window for regression | `20` |
| `LIGHTTRACK_ALERT_SCORE_MIN_SAMPLES` | min scores before a regression can trip | `8` |
| `LIGHTTRACK_ALERT_SCORE_DROP` | recent-vs-baseline mean drop that trips `score_drop` | `0.15` |

Attribution (below) reads the SQLite DB directly at `LIGHTTRACK_DB` (default `data/lighttrack.db`);
it is disabled when `LIGHTTRACK_DATABASE_URL` points at Postgres/Firestore.

The startup banner shows e.g. `alerts=webhook+ntfy+resend(2) (cooldown 3600s, error-spike >=5/300s,
score-drop >=15%)` (or `alerts=off`). The dedup key is `project:metric:window:scope` — a scoped cap
and a project-wide cap on the same metric+window track independent cooldowns.

## Webhook payload

```json
{
  "event": "limit_breach",
  "text":    "LightTrack alert: project '…' breached Calls/Hour limit — current … >= threshold … (…% of limit), action=…. Top spenders (in this window): gpt-4o (summarize) 62% ($3.1000), claude-sonnet 25% ($1.2500), gpt-4o-mini 13% ($0.6500).",
  "content": "… (same text) …",
  "breach":  { "rule_id", "project_id", "metric", "window", "action", "current", "threshold", "ratio", "breached", "warn_at", "warning", "scope" },
  "rejected_count": 4,
  "attribution": {
    "scope_note": null,
    "contributors": [
      { "label": "gpt-4o (summarize)", "cost_usd": 3.10, "share_pct": 62.0 },
      { "label": "claude-sonnet",      "cost_usd": 1.25, "share_pct": 25.0 },
      { "label": "gpt-4o-mini",        "cost_usd": 0.65, "share_pct": 13.0 }
    ]
  }
}
```

`text` is what **Slack** incoming webhooks render; `content` is what **Discord** webhooks render;
`breach` carries the structured fields for custom receivers. Point `LIGHTTRACK_ALERT_WEBHOOK` straight
at a Slack/Discord incoming-webhook URL, or at your own endpoint.

### Breach attribution ("what's burning the money?")

Every breach names the top-3 contributors that drove the spend over the breached window — each with
its share of window cost (%) and dollar figure, and (for a project-wide breach) the model annotated
with its dominant use-case, e.g. `gpt-4o (summarize)`. This is computed **inside the spawned delivery
task** from the existing `cost_summary_windowed` / `usecase_costs` rollups, so it adds **zero cost to
the ingest path**, and it's **best-effort**: if the rollup is empty or fails, the alert still delivers
without the `attribution` block (and without the "Top spenders" sentence).

For a **scoped** rule the attribution is *within* the scope, and `attribution.scope_note` states which:

- a **model** cap (`scope model=gpt-4o`) → top **use-cases** of that model;
- a **use-case** cap (`scope name=summarize`) → top **models** serving that use-case;
- a **provider** cap (`scope provider=openai`) → top **models** of that provider.

When a scoped window has no attributable spend, `contributors` is empty and `scope_note` says so
(the message reads `Top spenders: none attributable (scope …: no attributable spend in window)`).

> Attribution reads the SQLite cost rollups directly and is therefore **SQLite-only**; on a
> Postgres/Firestore backend (`LIGHTTRACK_DATABASE_URL`) the breach still delivers, just without the
> `attribution` block.

`rejected_count` is present for an enforcing (`throttle`/`block`) breach: how many ingest attempts
that cap has turned away (429'd) in the current rolling window.

The same channels also deliver **forecast alerts** (`"event": "forecast_alert"`, see
`docs/PREDICTIVE.md` / the forecast module) and **relay dead-letter alerts**
(`"event": "relay_task_dead"`, see `docs/RELAY.md`) — fired when a relay task exhausts its
attempts or its device vanishes past the retry envelope:

```json
{
  "event": "relay_task_dead",
  "text": "LightTrack alert: relay task '…' (xprice/…) in project '…' dead-lettered after N attempt(s) — …",
  "content": "… (same text) …",
  "task": { "id", "project_id", "action_type", "source", "attempts", "error" }
}
```

## ntfy

POSTs the message as the body to the topic URL (e.g. `https://ntfy.sh/my-lighttrack`), with headers
`Title: LightTrack limit breach`, `Tags: warning`, `Priority: high`.

## Notes

- Config is **server-global**; the payload carries `project_id` so one receiver can route per project.
  (Per-project alert routing would need a schema/Store change — deferred.)
- Dedup state is **in-memory**: it resets on restart, and multiple API instances each dedup
  independently, so a horizontally-scaled deployment may emit up to one alert per instance per window.
- `action` (`alert` / `throttle` / `block`) doesn't affect alert delivery — the breach is delivered
  regardless, including when a `throttle`/`block` breach also rejects the ingested event with 429.
