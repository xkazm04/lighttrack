# LightTrack

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
- **Notify** on limit breaches and score regressions.
- **Visualize** with a provisioned **Grafana** dashboard over the Postgres store.
- **Query from agents** via a built-in **MCP server** — rendered tables + slash-command workflows in
  Claude Code (or any MCP client).

## Install

### Container (published & public)
```bash
docker run -p 8787:8787 -v lt-data:/data ghcr.io/xkazm04/lighttrack:v0.0.4
curl localhost:8787/health        # -> ok
```
The image bundles **all backends** (SQLite by default; set `LIGHTTRACK_DATABASE_URL` for
Postgres/Firestore) and **all binaries** (`lighttrack-api`, `lt-runner`, `lt-mcp`, `lt`).
Pin a version tag (`:v0.0.2`) — there is no `:latest`.

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
**v0.0.2 — early but functional, and published.** Implemented: the core data plane
(events / cost / limits / scores), **all three store backends** (SQLite / Postgres / Firestore), the
multi-provider judge + benchmark engine, the **three client SDKs**, the MCP server, the operator CLI,
and the deploy assets above (Compose / Helm / Terraform / installers / GHCR image). Still planned:
DuckDB / libSQL / BigQuery backends, AWS Terraform, scheduled online sampling. See
[`docs/ROADMAP.md`](docs/ROADMAP.md).

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
`lt-mcp` is an MCP server over the API: **19 read tools** (events / costs / margin / scores / limits /
prices / projects / benchmarks + runs / datasets + items / rubrics / jobs) plus **9 write tools** (enqueue runs,
create project/dataset/rubric/benchmark/limit, `put_price`). Writes are **off by default**, gated behind
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
