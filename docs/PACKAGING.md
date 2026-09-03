# LightTrack — Packaging, Multi-Database & Multicloud Deployment

Design for making LightTrack trivially reusable: run it on a developer's **favorite database** (from
local-lite SQLite to managed cloud Postgres and beyond) and **deploy it to any cloud** (AWS, GCP, Azure)
or a laptop with minimal effort.

Status: **design**. Reframes/extends the original GCP-only Phase 5. Proposes Phase 5 sub-steps 5a–5f.

## Goals & non-goals
**Goals**
1. *Bring your own database.* One config value selects the backend; local-lite and every major managed DB work.
2. *Deploy anywhere.* One build artifact + layered tooling lands on AWS / GCP / Azure / k8s / a laptop.
3. *Stay lean.* No per-cloud rewrite; the domain core never imports a vendor SDK.

**Non-goals**: a bespoke control plane; coupling to one vendor; supporting every exotic DB on day one.

## Core principle — ports & adapters · one artifact · one DSN
- The **domain core** (events, cost, limits, scoring, benchmarks) imports **no** cloud/DB SDK.
- Every external dependency sits behind a **trait ("port")**; concrete **adapters** are chosen at runtime
  from config. This is hexagonal / ports-and-adapters.
- Ship **one container image** (+ static binaries). Select the database with **one
  `LIGHTTRACK_DATABASE_URL` DSN**; select cloud services with env vars.
- **Keystone already exists:** the [`Store`](../crates/store/src/lib.rs) trait already abstracts
  persistence (SQLite impl shipped). Multicloud = *more adapters* + the *same pattern* for
  queue / secrets / blob / notify.

---

## 1. Database portability ("bring your own DB")

### 1.1 `Store` trait → backend adapters
| Tier | Backends | Adapter | Status |
|---|---|---|---|
| Local-lite | **SQLite** *(shipped)*, DuckDB, libSQL/**Turso** | `SqliteStore` / `SqlStore` / `DuckStore` | SQLite done |
| Universal OLTP | **Postgres** (RDS / Cloud SQL / Azure DB / **Neon** / **Supabase**), MySQL | `SqlStore` (sqlx) | proposed 5a |
| GCP-native config | **Firestore** | `FirestoreStore` | full-scope (kept) |
| Analytical (scale) | BigQuery, ClickHouse, Athena/Redshift, Snowflake | `EventSink` (optional) | optional |
| Lakehouse (neutral) | Parquet/Iceberg on S3 / GCS / Azure Blob | via `object_store` / `opendal` | optional |

### 1.2 The 80/20 — one `SqlStore` over **sqlx** (SQLite + Postgres)
**Postgres is the universal cloud database** (every cloud has managed Postgres, plus cloud-neutral
serverless options). **SQLite** covers local-lite. `sqlx` speaks both (and MySQL) with one
compile-checked query layer — so a single `SqlStore` adapter covers ~80% of the matrix. Dialect care:
- Timestamps: keep the fixed-width RFC3339 string convention (works in both) or `TIMESTAMPTZ` on PG.
- Upsert: `INSERT … ON CONFLICT … DO UPDATE` exists in both SQLite and Postgres.
- JSON columns: `TEXT` (SQLite) vs `JSONB` (PG) — abstract via the serde (de)serialize we already do.
- Booleans/ints: normalize at the adapter boundary (we already map `bool ↔ INTEGER`).

### 1.3 DSN-driven selection + auto-migrate
- `LIGHTTRACK_DATABASE_URL` scheme picks the backend: `sqlite://./data/lt.db`,
  `postgres://user:pass@host/db`, `duckdb://…`, `bigquery://project/dataset`.
- **Auto-run migrations on startup** (idempotent) so a user points at an empty DB and it just works.
- Migrations: a `migrations/` dir applied by `sqlx migrate` (or `refinery`) for SQL dialects; the
  BigQuery DDL (`schema/bigquery`) stays separate, as today.

### 1.4 Analytical sinks (optional, for scale)
Split the *analytical* path from the *OLTP/config* path when volume warrants: an `EventSink` trait
(append-heavy) targets BigQuery / ClickHouse / Athena / Parquet-on-object-storage, while `Store`
(config + recent reads) stays on Postgres/SQLite. **DuckDB** locally mirrors the columnar cloud SQL, so
queries port. Small deployments skip this entirely — Postgres does both.

### 1.5 Cloud-neutral managed DBs (the "instant reuse" answer)
**Neon** and **Supabase** (serverless Postgres, generous free tiers) and **Turso** (libSQL/edge) run
*regardless of where the app is hosted* — a dev can point LightTrack at a free Neon DB from any cloud or
their laptop. Document these as first-class quickstart targets.

### 1.6 Parity guarantee — Store **conformance tests**
Write the `Store` trait's behavioral tests **once**, parametrized over backends, and run them against
each adapter in CI using the `testcontainers` crate (spins up Postgres/ClickHouse in Docker). This is
what makes "any DB" trustworthy — it catches dialect drift before users do.

---

## 2. Cloud-neutral internal services (same trait pattern)
| Concern | Neutral default | GCP | AWS | Azure | Rust crate(s) |
|---|---|---|---|---|---|
| Queue / jobs | **DB-backed `jobs` table** (Phase 3.6d) | Pub/Sub | SQS | Service Bus | sqlx / cloud SDKs |
| Object / blob | local fs | GCS | S3 | Blob | **`opendal`** / `object_store` (one API, all) |
| Secrets | env / file | Secret Mgr | Secrets Mgr / SSM | Key Vault | `SecretProvider` trait |
| Notify | webhook / ntfy | Pub/Sub→Fn | SNS | Event Grid | `Notifier` trait *(seam exists)* |
| Schedule | cron / `lt-runner serve` | Cloud Scheduler | EventBridge | Logic Apps | — |

- The **DB-backed jobs queue** is the portability hero: no managed queue required, so the app behaves
  identically on a laptop and any cloud. Managed-queue adapters are optional optimizations.
- **`opendal`** gives one object-storage API across S3 / GCS / Azure Blob / local fs — ideal for dataset
  and report artifacts.
- TLS is handled by the platform or a reverse proxy (Caddy/Traefik); the app stays plain HTTP behind it,
  so it's identical everywhere.

---

## 3. Build once, run anywhere
- **Multi-stage Dockerfile** → static `musl` binary → `distroless`/`scratch` image (~10–20 MB). Rust makes
  this trivial; the image runs on *every* container runtime.
- **Multi-arch** (amd64 + arm64) via `docker buildx` — clouds are increasingly arm (Graviton / Ampere /
  Cobalt), which is cheaper.
- Publish to **GHCR**: `docker run ghcr.io/xkazm04/lighttrack`.
- **`docker-compose.yml`** brings up the whole stack locally (api + runner + Postgres + Grafana) in one
  command.
- Point the k8s probes at the endpoint shaped for each: `/health/live` for liveness (observes nothing
  outside the process, so a slow store never restarts the pod) and `/health/ready` for readiness (round-trips
  the store, so an unreachable dependency drops the pod out of the Service). `/health` stays the operator
  rollup. One endpoint for both probes is one wrong answer — see `crates/api/src/health.rs`. Optional
  supply-chain extras: SBOM (`syft`), signing (`cosign`).

---

## 4. Deploy tooling — tiered, so easy stays easy
0. **Local:** `docker compose up`.
1. **One-command cloud** (cheapest serverless-container per cloud): `gcloud run deploy` ·
   `aws apprunner` / `copilot` · `az containerapp up`. Plus README **deploy buttons** (Render / Railway /
   Fly, "Run on Google Cloud", CloudFormation launch-stack URL, "Deploy to Azure" ARM button).
2. **Production IaC:** **Terraform / OpenTofu** modules `deploy/terraform/modules/{aws,gcp,azure}` with a
   *common variable interface* (`image`, `database_url`, `env`, `secrets`) — each stands up
   container-runtime + managed Postgres + secrets + scheduler. Swap the module, keep the inputs.
3. **Kubernetes:** one **Helm chart** runs on EKS / GKE / AKS / any cluster (values for DSN, replicas,
   secrets, autoscaling).

| Cloud | Easiest container svc | Managed Postgres | Secrets | Scheduler |
|---|---|---|---|---|
| GCP | Cloud Run (scale-to-zero) | Cloud SQL / Neon | Secret Manager | Cloud Scheduler |
| AWS | App Runner / Fargate | RDS / Aurora Serverless | Secrets Mgr / SSM | EventBridge |
| Azure | Container Apps | Azure DB for PostgreSQL | Key Vault | Logic Apps / Timer |

*(A function-style path is possible — `cargo-lambda`→Lambda, container Cloud Functions, Azure custom
handler — but container-first is simpler and portable; keep functions as an optional adapter.)*

---

## 5. Distribution & install UX ("instantly reuse")
- **`cargo-dist`** generates prebuilt binaries + a `curl | sh` installer + Homebrew tap + Scoop (Windows)
  + GitHub Releases artifacts.
- **`lighttrack init`** scaffolding command: writes `.env`, picks a backend, drops a `docker-compose.yml`
  or Helm `values.yaml`.
- **Devcontainer + Codespaces** config for instant cloud dev.
- GHCR image for `docker run` users.

---

## 6. Config & secrets (12-factor)
- All config via env (`LIGHTTRACK_*`), one `LIGHTTRACK_DATABASE_URL` for the DB. A `config` layer (e.g.
  `figment`) merges env + optional file.
- A `SecretProvider` trait: default reads env/file; cloud adapters read Secret Manager / Secrets Manager /
  Key Vault — or simply let the platform inject secrets as env (simplest, fully neutral).

---

## 7. Dashboards portability
Ship a **Grafana dashboard JSON** over Postgres/ClickHouse (cloud-neutral) alongside the GCP-only Looker
Studio option. Grafana runs anywhere (incl. the compose stack), so the "do more with the data" promise
isn't tied to one vendor. Metabase/Superset are drop-in alternatives.

---

## 8. Relationship to `DECISIONS.md` (proposed evolution — to confirm)
The original cloud decisions were GCP-specific. The multicloud pivot:
| Original (D2/D3) | Proposed (multicloud) |
|---|---|
| Config/OLTP store = Firestore (only) | **Postgres** default (universal); **Firestore kept** as the GCP-native option; SQLite local |
| Analytical store = BigQuery (required) | BigQuery = **optional** `EventSink`; Postgres suffices small-scale |
| Dashboards = Looker Studio (GCP) | add **Grafana** (neutral) alongside |
| Compute = Cloud Run | **any serverless container** (Cloud Run / App Runner / Container Apps) |
| Deploy = gcloud | container image + Terraform modules + Helm + one-command scripts |

These are proposals; ratify before implementing 5a (they change the schema/wiring).

---

## 9. Phase 5 — reorganized as 5a–5f
- **5a — `SqlStore` (SQLite + Postgres via sqlx)** behind the `Store` trait + `LIGHTTRACK_DATABASE_URL` +
  auto-migrate + **conformance tests** across both. *(The pivotal step; no cloud account needed — test PG
  via testcontainers.)*
- **5b — Container:** multi-stage Dockerfile (musl/distroless) + `docker-compose.yml` + GHCR multi-arch CI.
- **5c — Service adapters:** `Queue` (DB-jobs default), `BlobStore` (`opendal`), `SecretProvider`,
  `Notifier` (webhook) behind traits; cloud impls optional/feature-flagged.
- **5d — Terraform modules** (`aws`/`gcp`/`azure`, common interface) + one-command scripts + deploy buttons.
- **5e — Helm chart** (EKS/GKE/AKS/any k8s).
- **5f — Distribution & dashboards:** `cargo-dist` install paths + Grafana dashboard JSON + a backend
  **compatibility matrix** in docs.

## 10. Proposed repo layout
```
deploy/
  docker/Dockerfile
  compose/docker-compose.yml
  helm/lighttrack/{Chart.yaml,values.yaml,templates/}
  terraform/
    modules/{aws,gcp,azure}/        # common vars: image, database_url, env, secrets
    examples/{aws,gcp,azure}/
dashboards/grafana/lighttrack.json
migrations/                          # sqlx migrations (SQL dialects)
crates/store/src/{sqlite.rs,sql.rs,bigquery.rs}   # adapters behind the Store trait
```

## 11. Open decisions (confirm before 5a)
1. **Postgres as the cross-cloud default** config/OLTP store, with **Firestore kept** as the GCP-native
   option (full-scope requirement — both supported behind the `Store` trait). *(confirmed)*
2. **Query layer:** `sqlx` (raw SQL, compile-checked, lightweight — recommended) vs SeaORM/Diesel (ORM).
3. **IaC tool:** Terraform vs **OpenTofu** (OSS, license-clean — recommended) vs Pulumi (no Rust SDK).
4. **Analytical sink in v1** (BigQuery/ClickHouse `EventSink`) or defer until scale demands it?

## References (well-established tooling)
sqlx · SeaORM/Diesel · testcontainers-rs · `opendal` / `object_store` · cargo-dist · cargo-lambda ·
Terraform/OpenTofu · Helm · Neon / Supabase / Turso · Grafana.
