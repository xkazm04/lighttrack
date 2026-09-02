# Alerts

When something happens that an operator needs to know about — an ingested event trips a project
limit, a spend forecast crosses a budget, a project's failures spike, a rubric's scores regress, a
relay task dead-letters, a benchmark run finishes, or the caps turn traffic away — LightTrack
**writes an alert row** and then delivers it to the channels that project routes to.

The row comes first, and that ordering is the whole design. Delivery is best-effort and **off the
request path** (a spawned task), so a slow or down sink never delays or fails ingest; but the
*record* is not best-effort. `GET /v1/alerts` answers what fired, whether each delivery landed, who
acknowledged it, and what came of it.

## The seven-plus-one kinds

| `kind` | Fires when | Default severity |
|---|---|---|
| `limit_breach` | a limit rule breached | `critical` |
| `limit_warning` | a rule crossed its `warn_at` fraction without breaching | `warning` |
| `forecast_alert` | a projected budget breach or margin erosion (`critical` when ≤3 days out) | `warning` |
| `relay_task_dead` | a relay task exhausted its attempts, or its device vanished | `critical` |
| `error_spike` | a project's failed calls crossed the threshold inside the rolling window | `warning` |
| `score_drop` | a (project, rubric) recent mean regressed below its baseline | `warning` |
| `bench_run` | a benchmark run finished (the CI gate contract's completion hook) | `info` |
| `ingest_rejected` | periodic flush: how many ingest attempts the caps turned away | `info` |
| `prompt_canary_regressed` | a prompt's canary label is measurably worse than its production label | `warning` |

`prompt_canary_regressed` is the one alert that fires **after** a promotion, which is where the
prompt registry used to stop looking. It requires both an evidence floor (`min_n` verdicts on each
side) and non-overlapping ~95% intervals past the policy's `max_drop` band, so it cannot be tripped
by noise; the payload carries both means, both intervals and both counts, plus whether an
`auto_revert` already moved the label back. Off unless `LIGHTTRACK_PROMPT_CANARY_SWEEP_SECS` is set
and the prompt carries a policy — see `BENCHMARK_FRAMEWORK.md` §2.

`ingest_rejected` is an **alert row and never an event**. A rejected call is deliberately not stored
as an event — it would corrupt the usage and cost rollups every cap is evaluated against — so the
fact that a cap turned traffic away is recorded here instead.

## Deduplication is durable

Each alert carries a `dedup_key` (`project:metric:window:scope` for a breach, `warn:…` for the
matching warning, `forecast:…`, `relay-dead:<id>`, `error-spike:<project>`, `score-drop:…`,
`bench-run:<benchmark>:<status>`). `Store::insert_alert_dedup` admits or suppresses in **one atomic
step** — a SQLite transaction, a Postgres transaction-scoped advisory lock on the key, a Firestore
guard document driven by `exists=false` / `updateTime` compare-and-set.

That is what makes a horizontally-scaled deployment alert **once**. Two Cloud Run replicas that both
evaluate the same breach in the same second each pass their own in-process cooldown map (which is
only a write-through cache), and exactly one gets `Admitted`. A suppressed alert leaves no row: the
ledger counts incidents, not attempts.

The warning key is deliberately distinct from the breach key, so an approaching-limit warning never
suppresses the later breach for the same rule.

Two things stay in process memory on purpose, and neither is a fact anyone can point at: the
**error window** and the **score window**. They are rate detectors — "have I seen five failures in
five minutes" is a question about this replica's recent traffic — and putting them in the store
would mean a write on the ingest path for every failed call. The alert that results is durable like
any other.

## Reading the ledger

```
GET  /v1/alerts?project=&kind=&since=&acked=&limit=&cursor=   # read scope (own project) or admin
POST /v1/alerts/:id/ack          {"by": "oncall-mia"}          # manage scope or admin
POST /v1/alerts/:id/resolution   {...}                         # admin (the responder's door)
```

`since` takes an RFC3339 instant or a relative `30m` / `24h` / `7d`. The response is
`{"alerts": [...], "next_cursor": "…"}`; pass `next_cursor` back as `cursor` to page. `acked=false`
is the on-call view.

Acknowledgement and resolution are **separate facts**: someone *saw* it, and something *came of it*.
Neither silences anything — the alert stays in the ledger and its cooldown is unaffected.

From the CLI: `lt alerts list --open --since 7d` and `lt alerts ack <id> --by oncall`. Over MCP:
`list_alerts` (read-only) and `ack_alert` (gated behind `LIGHTTRACK_MCP_ALLOW_WRITES`). Alert
*channels* are deliberately not exposed over MCP — creating one returns a signing secret once, and
that would land verbatim in an agent's transcript.

## Routing

An alert for project *P* goes to **P's own channels ∪ the global channels**, each narrowed by its
severity floor and its kind filter.

The env-configured destinations below are synthesised at startup as **global channel rows** — they
are never persisted, carry no severity floor and no kind filter, and therefore reproduce the
pre-routing behaviour exactly. A deployment that has configured nothing per-project routes as it
always did.

```
GET    /v1/projects/:id/alert-channels          # admin; the EFFECTIVE set (own ∪ global), redacted
PUT    /v1/projects/:id/alert-channels          # admin; create
DELETE /v1/projects/:id/alert-channels/:cid     # admin
POST   /v1/alert-channels/:id/test              # admin; send a real, signed test alert
```

A create body:

```json
{ "kind": "webhook", "target": "https://hooks.example.com/lt",
  "min_severity": "warning", "kinds": ["limit_breach", "error_spike"], "signed": true }
```

`kinds: []` (or absent) means every kind. With `"signed": true` the server mints the secret and
returns it **once**, in that response only — the API-key show-once pattern.

## Signature contract

Every webhook delivery from a channel with a signing key carries:

```
X-LightTrack-Signature: t=1756732800,v1=<hex hmac-sha256 over "t.body">
```

`body` is the exact bytes of the request body. To verify:

1. derive the key: `key = sha256(secret)`, lowercase hex — LightTrack stores only this derived key,
   never the secret you were shown;
2. compute `HMAC-SHA256(key, "<t>.<raw body>")` as lowercase hex;
3. compare against each `v1=` value in constant time;
4. **reject a `t` outside your tolerance** (the responder uses 300s). The timestamp is inside the
   signed string, so this is what stops a captured body being replayed later.

During a rotation the header carries a `v1=` for the current key *and* one for the previous, so a
receiver on either side of the change verifies. Set `LIGHTTRACK_ALERT_WEBHOOK_SECRET` to sign the
env-configured webhook's deliveries.

The implementation lives in `lighttrack_core::alert_sign` and is shared by the sender (the API) and
the receiver (the responder): a signature scheme with two implementations is a scheme with two
behaviours.

## Destination rules

A webhook or ntfy destination must be:

- **`https://`.** Plaintext would put the alert body — project names, models, spend — on the wire in
  the clear. `http://localhost` is allowed only with `LIGHTTRACK_ALERT_ALLOW_LOOPBACK=1`.
- **a public address.** *Every* address the host resolves to is checked and refused if loopback,
  private (10/8, 172.16/12, 192.168/16), link-local (169.254/16 — where every cloud parks its
  instance-metadata service), unique-local, CGNAT (100.64/10), or not a unicast host. A hostname
  resolving to one public and one private address is the standard rebinding trick, so one bad answer
  refuses the destination.

The check runs at **configure** time (a bad channel is a 400 that says why) **and before every
delivery**, so a hostname re-pointed at `10.0.0.5` after configuration does not become a door.
Redirects are not followed (`redirect::Policy::none()`) — a 302 to `169.254.169.254` would otherwise
walk straight past all of the above — and the response body is read to a 2 KB cap.

## Configure (env on the API)

`alerts/mod.rs::from_env` is the source of truth for these keys.

**Channels** — synthesised as global channel rows:

| Env | Meaning |
|-----|---------|
| `LIGHTTRACK_ALERT_WEBHOOK` | POST a JSON body to this URL (Slack/Discord/custom) |
| `LIGHTTRACK_ALERT_WEBHOOK_SECRET` | sign that webhook's deliveries (see above) |
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
| `LIGHTTRACK_ALERT_ALLOW_LOOPBACK` | dev only: permit `http://localhost` destinations | unset |
| `LIGHTTRACK_ALERT_REJECTION_FLUSH_SECS` | how often the rejection ledger flushes (`0` = off) | `900` |

The startup banner shows e.g. `alerts=webhook(signed)+ntfy+resend(2) (cooldown 3600s, error-spike
>=5/300s, score-drop >=15%), ledger: on`.

## Webhook payload

The stored `payload` **is** the delivered body, plus `alert_id` added on the wire:

```json
{
  "event": "limit_breach",
  "alert_id": "6f1c…",
  "text":    "LightTrack alert: project '…' breached Calls/Hour limit — current … >= threshold … (…% of limit), action=…. Top spenders (in this window): gpt-4o (summarize) 62% ($3.1000), claude-sonnet 25% ($1.2500), gpt-4o-mini 13% ($0.6500).",
  "content": "… (same text) …",
  "subject": "LightTrack: limit breach in '…'",
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
`subject` is the email subject (a webhook receiver ignores it); the kind-specific block carries the
structured fields for custom receivers. `alert_id` is what lets a receiver answer back — the
responder POSTs its diagnosis to `/v1/alerts/<alert_id>/resolution`.

### Breach attribution ("what's burning the money?")

Every breach names the top-3 contributors that drove the spend over the breached window — each with
its share of window cost (%) and dollar figure, and (for a project-wide breach) the model annotated
with its dominant use-case, e.g. `gpt-4o (summarize)`. It is computed **inside the spawned delivery
task** from the existing `cost_summary_windowed` / `usecase_costs` rollups, so it adds **zero cost to
the ingest path**, and it is **best-effort**: an empty or failed rollup delivers the alert without
the `attribution` block.

Attribution reads the store the API is already using, so it works on **every backend that serves the
windowed cost rollups** — Postgres and Firestore included. (It used to open a second SQLite handle
from a path re-derived from env, which meant it was simply absent on the backends carrying
production traffic.)

For a **scoped** rule the attribution is *within* the scope, and `attribution.scope_note` states
which:

- a **model** cap (`scope model=gpt-4o`) → top **use-cases** of that model;
- a **use-case** cap (`scope name=summarize`) → top **models** serving that use-case;
- a **provider** cap (`scope provider=openai`) → top **models** of that provider.

When a scoped window has no attributable spend, `contributors` is empty and `scope_note` says so.

`rejected_count` is present for an enforcing (`throttle`/`block`) breach: how many ingest attempts
that cap has turned away (429'd) in the current rolling window.

The same channels also deliver **forecast alerts** (`"event": "forecast_alert"`) — pre-emptive
budget-breach and margin-erosion warnings, gated by the evidence floor described in
[`docs/PREDICTIVE.md`](PREDICTIVE.md) and de-duplicated on
`forecast:<project>:<kind>:<subject>:<severity>` — and **relay dead-letter alerts**
(`"event": "relay_task_dead"`, see `docs/RELAY.md`) — fired when a relay task exhausts its
attempts or its device vanishes past the retry envelope:

```json
{
  "event": "relay_task_dead",
  "alert_id": "…",
  "text": "LightTrack alert: relay task '…' (xprice/…) in project '…' dead-lettered after N attempt(s) — …",
  "content": "… (same text) …",
  "task": { "id", "project_id", "action_type", "source", "attempts", "error" }
}
```

## ntfy

POSTs the message as the body to the topic URL (e.g. `https://ntfy.sh/my-lighttrack`), with headers
`Title` (the alert's subject), `Tags: warning`, `Priority: high`.

## Backend parity

`Surface::Alerts` and `Surface::AlertRouting` are declared by all three backends (see
`docs/PARITY.md`). A backend that refused them would still deliver alerts, but nothing would record
what fired or whether it landed, and deduplication would fall back to each replica's own memory —
`GET /v1/capabilities` says so in those words.

## Notes

- `action` (`alert` / `throttle` / `block`) doesn't affect delivery — the breach is delivered
  regardless, including when a `throttle`/`block` breach also rejects the ingested event with 429.
- A delivery outcome is recorded per channel, failures included: `delivered: []` on a listed alert
  means it reached **nobody**, and `lt alerts list` calls that out above the table.
