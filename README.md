<p align="center">
  <img src="logo.png" alt="LightTrack" width="160">
</p>

<h1 align="center">LightTrack</h1>

A lightweight, self-hosted **LLM observability + scoring** tool. Think Langfuse, but headless-first,
data-open (raw SQL over everything), and using **Claude Code headless (`claude -p`) as a pluggable
scoring/benchmark engine**.

One container image, one `LIGHTTRACK_DATABASE_URL` — runs on **SQLite, Postgres, or Firestore**, on
your laptop or any cloud.

## What it does
- **Track** LLM calls from your apps via drop-in **Python / TypeScript / Rust** SDKs — across
  **OpenAI, Anthropic, and Google (Gemini)**.
- **Cost** accounting per call / model / project, from a maintained, DB-backed price book.
- **Profit** per customer / product — net **revenue** (Stripe/Polar webhooks, or `lt-runner billing
  sync`) against LLM cost to surface unprofitable users (`GET /v1/margin`, `lt margin`,
  `/lighttrack:margin-report`).
- **Limits** per project (cost, calls, tokens over hour/day/month) that incoming traffic can trip →
  alerts + an advisory throttle flag apps/MCP can read.
- **Score & benchmark** traces with an LLM-as-judge run through `claude -p` (structured
  `--json-schema` verdicts); generate candidate outputs from OpenAI / Gemini / Anthropic.
- **Collective model intelligence** — opt in to publish a **k-anonymized digest** of your benchmark
  scorecards (aggregate quality/cost/latency per model × task type — no prompts, no ids, no customer
  data) to a shared hub, and read back a leaderboard built from other operators' *real* tasks.
- **Notify** on limit breaches and score regressions.
- **Visualize** with a provisioned **Grafana** dashboard over the Postgres store.
- **Query from agents** via a built-in **MCP server** — rendered tables + slash-command workflows in
  Claude Code (or any MCP client).

## Your first tracked event

Two commands, no signup, no config. This is the whole loop — send a call, get it back priced.

```bash
docker run -p 8787:8787 -v lt-data:/data ghcr.io/xkazm04/lighttrack:v0.0.6

curl -X POST localhost:8787/v1/events -H 'content-type: application/json' \
  -d '{"provider":"openai","model":"gpt-4o-mini","operation":"chat","status":"success",
       "usage":{"input":1000,"output":500}}'
# -> {"id":"…","project_id":"default","cost_usd":0.00045,…}   cost priced server-side

curl 'localhost:8787/v1/events?limit=1'
```

That `cost_usd` came from the DB-backed price book, not from you — which is the point. A fresh
instance starts in **dev mode**: no API key, and an event with no project lands in a `default`
project so you can see something work before configuring anything.

**Before you point a real app at it**, set `LIGHTTRACK_AUTH_MODE=enforced` and a
`LIGHTTRACK_ADMIN_KEY`, then mint a per-project key (`POST /v1/projects`, then
`POST /v1/projects/<id>/keys`). Dev mode accepts any bearer token as admin and says so loudly at
startup. From your app, the SDKs below need `LIGHTTRACK_PROJECT` (or a project key, which implies
one) — without either, an event has nothing to attribute to.

## Install

### Container (published & public)
```bash
docker run -p 8787:8787 -v lt-data:/data ghcr.io/xkazm04/lighttrack:v0.0.6
curl localhost:8787/health        # -> ok
```
The image bundles **all backends** (SQLite by default; set `LIGHTTRACK_DATABASE_URL` for
Postgres/Firestore) and **all binaries** (`lighttrack-api`, `lt-runner`, `lt-mcp`, `lt`); it is built
for linux/amd64 + linux/arm64. A `:latest` tag is published and currently tracks the newest release —
**pin an explicit version tag** in anything you deploy so a new release can't move under you. Tags
track the [Releases](https://github.com/xkazm04/lighttrack/releases) page.

### Prebuilt binaries
Download a tarball/zip from [Releases](https://github.com/xkazm04/lighttrack/releases), or install the
latest in one line:
```bash
curl -fsSL https://raw.githubusercontent.com/xkazm04/lighttrack/main/deploy/install.sh | sh    # Linux / macOS
```
```powershell
irm https://raw.githubusercontent.com/xkazm04/lighttrack/main/deploy/install.ps1 | iex          # Windows
```

### From source
```powershell
cargo build --release
target/release/lighttrack-api     # binds 127.0.0.1:8787 (override with LIGHTTRACK_BIND)
```

### Guided setup
Run **`/onboard`** in Claude Code from this repo — it walks you through picking a database + deploy
target, collects the credentials your choices need, then deploys and verifies for you.

## Supported tooling to integrate with

### App SDKs — send your LLM calls
Thin, **fire-and-forget** clients: non-blocking, best-effort, and they never throw into your app. They
wrap a provider response, normalize `{provider, model, usage}`, and POST to `/v1/events`; the server
derives the project from the API key and computes cost. Full docs: [`clients/`](clients/README.md).

| Language | Install | Notes |
|---|---|---|
| Python | `pip install ./clients/python` | stdlib only, background daemon thread |
| TypeScript / JS | `npm install` in `clients/typescript` (or vendor it) | zero-dep `fetch`, Node 18+/browser |
| Rust | path/git dep on `lighttrack-client` | reuses `lighttrack-core::LlmEvent` |

```python
from lighttrack import LightTrack
lt = LightTrack(source="my-app")               # env: LIGHTTRACK_URL / _KEY / _PROJECT
resp = openai_client.chat.completions.create(model="gpt-4o", messages=[...])
lt.track_openai(resp, latency_ms=120)          # model + usage → /v1/events; cost priced server-side
```

### LLM providers
| Provider | Used for | Key |
|---|---|---|
| Anthropic (`claude -p`) | judge engine + generation (default) | subscription OAuth or `ANTHROPIC_API_KEY` |
| OpenAI | candidate generation | `OPENAI_API_KEY` |
| Google Gemini | candidate generation | `GEMINI_API_KEY` |

### Billing providers — net revenue against cost
Wire a billing provider to turn cost into **margin**. A signed webhook
(`POST /v1/billing/stripe/webhook?project=<id>`, HMAC-verified — the signature is the auth, so no key
header) streams paid invoices/refunds in as normalized revenue; `lt-runner billing sync` backfills from
the provider API. Then `GET /v1/margin?by=customer|product` returns the revenue − LLM-cost rollup (judge
spend excluded), most-unprofitable first.

| Provider | Ingest | Secrets |
|---|---|---|
| Stripe | webhook (HMAC-SHA256/hex) + `billing sync` (backfill) | `LIGHTTRACK_STRIPE_WEBHOOK_SECRET`, `STRIPE_API_KEY` |
| Polar | webhook (Standard Webhooks / base64) | `LIGHTTRACK_POLAR_WEBHOOK_SECRET` |

Point each provider's webhook at `…/v1/billing/<provider>/webhook?project=<id>`.

### Databases — select with `LIGHTTRACK_DATABASE_URL`
| Backend | Selector | Best for |
|---|---|---|
| SQLite (bundled) | `LIGHTTRACK_DB=./data/lt.db` (default) | local / single VM |
| Postgres | `postgres://…` — Neon, Supabase, RDS, Cloud SQL, Azure DB | cross-cloud default |
| Firestore | `firestore://<project-id>` | GCP-native |

SQLite runs in WAL mode, so the database is three files (`lt.db`, `lt.db-wal`, `lt.db-shm`) — back up
and mount the **directory**, not the single file. Reads come from a connection pool
(`LIGHTTRACK_SQLITE_READ_POOL`, default 4) so dashboard queries don't queue behind ingest.

### Deploy targets
| Target | How |
|---|---|
| Docker Compose | `deploy/compose/` — SQLite, or `docker-compose.postgres.yml` (Postgres + Grafana) |
| Kubernetes | `helm install lighttrack deploy/helm/lighttrack -f values.yaml` |
| GCP / Azure | Terraform modules in `deploy/terraform/modules/{gcp,azure}` (Cloud Run / Container Apps) |
| Bare binary | install script above, or `cargo build --release` |

### Observability & agents
- **Grafana** — provisioned datasource + dashboard JSON in [`dashboards/grafana/`](dashboards/grafana)
  (over the Postgres store; brought up by the Postgres compose file).
- **MCP** — `lt-mcp` exposes rendered read tools + slash-command workflows to Claude Code / any MCP
  client (see below).

## Status
**v0.0.6 — early but functional, and published** (latest tag on the
[Releases](https://github.com/xkazm04/lighttrack/releases) page). Implemented: the core data plane
(events / traces / cost / limits / scores), **all three store backends** (SQLite / Postgres /
Firestore), the multi-provider judge + benchmark engine, scheduled online sampling
(`lt-runner schedule`), the collective leaderboard, the **three client SDKs**, the MCP server, the
operator CLI, and the deploy assets above (Compose / Helm / Terraform / installers / GHCR image).
Still planned: DuckDB / libSQL / BigQuery backends, AWS Terraform, and applying the Helm/Terraform
assets against real cloud credentials. See [`docs/ROADMAP.md`](docs/ROADMAP.md).

## Layout
```
crates/core             event model, price book + cost calc, limits, scoring types
crates/store            Store trait + SQLite backend (bundled)
crates/store-pg         Postgres backend (sqlx)
crates/store-firestore  Firestore backend (REST, no gRPC)
crates/engine           judge + multi-provider generation (claude / openai / gemini)
crates/anon             dataset anonymization
crates/api              ingest + query REST service (axum)
crates/runner           judge/benchmark + queue worker (drives `claude -p`)
crates/mcp              MCP server (read tools + gated writes)
crates/cli              operator CLI (`lt`)
clients/                Python / TypeScript / Rust app SDKs
deploy/                 Dockerfile, Compose, Helm, Terraform, install scripts
dashboards/grafana/     provisioned datasource + dashboard
config/                 pricing.json, lighttrack.example.toml
schema/                 SQLite (local) + Postgres DDL
docs/                   architecture, data model, packaging, roadmap, decisions
```

## Use from Claude Code (MCP)
`lt-mcp` is an MCP server over the API: **28 read tools** (events + traces / costs + use-cases / margin +
forecast / scores / limits / prices / projects / benchmarks + runs + CI gate / datasets + items / rubrics /
prompt registry / jobs / collective leaderboard + digest) plus **15 write tools** (enqueue runs, record
scores, create project/dataset + items/rubric/benchmark, create/update/delete limit, prompt versions +
gated promotion, `put_price`). Writes are **off by default**, gated behind
`LIGHTTRACK_MCP_ALLOW_WRITES=1` on top of the API's admin checks; key-minting is deliberately not exposed.

A project-scoped [`.mcp.json`](.mcp.json) is committed, so after `cargo build` and starting the API on
`:8787`, open Claude Code in this repo and approve the `lighttrack` server — then ask *"what did project
qa-demo spend?"* or *"did the latest capitals-qa run regress?"*.

**Rendered output.** Tool results come back as compact **Markdown** — aligned tables, ✅/❌/⚠️ status
glyphs, and `▁▃▅▇` trend sparklines (cost rollups, benchmark leaderboards, limit status) — alongside the
raw object as `structuredContent` (each read tool also declares an `outputSchema`). The same render layer
powers the `lt` CLI (tables on a TTY; `--json` to opt out) and the `lt-runner bench` compare leaderboard.

**Slash commands.** The server ships MCP prompts that Claude Code surfaces as slash commands:
`/lighttrack:cost-report`, `/lighttrack:limit-check`, `/lighttrack:benchmark-leaderboard`,
`/lighttrack:score-triage`, `/lighttrack:recent-activity`, `/lighttrack:price-book`,
`/lighttrack:margin-report`.

- Windows path is `target/debug/lt-mcp.exe`; on Linux/macOS change it to `target/debug/lt-mcp`.
- In `enforced` auth mode, add `"LIGHTTRACK_KEY": "<admin-or-project-key>"` to the server's `env`.
- Equivalent manual registration: `claude mcp add lighttrack -- <abs-path-to>/lt-mcp.exe`.

## Key facts to remember
- **Claude Code billing changes 2026-06-15:** headless `claude -p` no longer draws on the normal
  subscription — it meters against a separate monthly **Agent SDK credit** (Max 20x = $200/mo, no rollover)
  at API rates. LightTrack's judge runs against that credit. See [`docs/DECISIONS.md`](docs/DECISIONS.md).
- The **judge engine is unbudgeted** by design; **limits apply only to monitored (incoming) traffic**.

## License
Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option — the
same `MIT OR Apache-2.0` the workspace and client SDK manifests declare.
