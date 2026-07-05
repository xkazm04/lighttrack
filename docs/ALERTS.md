# Breach alerts

When an ingested event trips a project limit (cost / calls / tokens × hour / day / month), the API
logs an `[ALERT]` line and — if configured — delivers the breach to a **webhook** and/or **ntfy**
endpoint.

Delivery is **best-effort** and **off the request path** (a spawned task), so a slow or down sink
never delays or fails ingest. Alerts are **deduplicated** per `(project, metric, window)` with a
cooldown, so a sustained breach (which trips on every ingest until the rolling window clears) doesn't
spam the channel.

## Configure (env on the API)

| Env | Meaning |
|-----|---------|
| `LIGHTTRACK_ALERT_WEBHOOK` | POST a JSON body to this URL on breach |
| `LIGHTTRACK_ALERT_NTFY` | POST a text body to this ntfy topic URL on breach |
| `LIGHTTRACK_ALERT_COOLDOWN_SECS` | re-alert window per (project, metric, window); default `3600` |

Either or both channels may be set. The startup banner shows e.g. `alerts=webhook+ntfy (cooldown 3600s)`
(or `alerts=off`).

## Webhook payload

```json
{
  "event": "limit_breach",
  "text":    "LightTrack alert: project '…' breached Calls/Hour limit — current … >= threshold … (…% of limit), action=…",
  "content": "… (same text) …",
  "breach":  { "rule_id", "project_id", "metric", "window", "action", "current", "threshold", "ratio", "breached" }
}
```

`text` is what **Slack** incoming webhooks render; `content` is what **Discord** webhooks render;
`breach` carries the structured fields for custom receivers. Point `LIGHTTRACK_ALERT_WEBHOOK` straight
at a Slack/Discord incoming-webhook URL, or at your own endpoint.

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
