# LightTrack on Google Cloud Run — one command, ~free

Deploys the LightTrack API as a **scale-to-zero** Cloud Run service. With `min-instances=0` you pay
nothing while idle; Cloud Run's always-free quota (2M requests, 180k vCPU-s, 360k GiB-s per month)
comfortably covers low-traffic test apps (5–10 apps × 10–100 calls/hr).

## Prerequisites
- `gcloud` CLI installed and authenticated (`gcloud auth login`) with a project that has billing
  enabled. (Always-Free quotas still apply — you won't be charged while under them.)
- `curl` (the bash script uses it for the health check).

## Quick start
```bash
# from the repo root (bash / Git Bash / WSL):
deploy/cloudrun/deploy.sh --project YOUR_PROJECT
```
```powershell
# Windows PowerShell:
deploy\cloudrun\deploy.ps1 -Project YOUR_PROJECT
```
The script enables the needed APIs, stores a generated admin key in Secret Manager, **mirrors the
published public image** (`ghcr.io/xkazm04/lighttrack`) into your project's Artifact Registry (Cloud
Run only deploys from Artifact Registry / gcr.io, not external registries), deploys it, and curls
`/health`. It prints the service URL and the admin key (**shown once** — save it).

## Storage: ephemeral SQLite vs durable Postgres
Cloud Run has an ephemeral filesystem, so **SQLite data is lost on every cold start / new revision**.
That's fine for a first smoke test, but for anything real pass a Postgres DSN — a free
[Neon](https://neon.tech) database is the recommended pairing:

```bash
deploy/cloudrun/deploy.sh --project YOUR_PROJECT \
  --database-url "postgres://user:pass@ep-xxx.region.aws.neon.tech/lighttrack?sslmode=require"
```
The DSN is stored in Secret Manager and injected as `LIGHTTRACK_DATABASE_URL`; the app auto-migrates
on startup. Neon scale-to-zero + Cloud Run scale-to-zero = a genuinely $0 idle stack.

## Build from source (forks / local changes)
By default the script deploys the prebuilt public image. To build *your* working tree instead:
```bash
deploy/cloudrun/deploy.sh --project YOUR_PROJECT --build
```
This runs `cloudbuild.yaml` (Docker build of `deploy/docker/Dockerfile`) into an Artifact Registry
repo, then deploys that image. A release build of the Rust workspace takes ~15–20 min on the default
Cloud Build machine; see `cloudbuild.yaml` to opt into a faster (paid) machine.

## Options
| Flag (sh / ps1) | Default | Notes |
|---|---|---|
| `--project` / `-Project` | active gcloud project | required |
| `--region` / `-Region` | `us-central1` | a Tier-1 region keeps you in the free tier |
| `--database-url` / `-DatabaseUrl` | _(none → SQLite)_ | Postgres DSN (Neon/Supabase/Cloud SQL) |
| `--admin-key` / `-AdminKey` | generated | admin key for `/v1` management routes |
| `--auth-mode` / `-AuthMode` | `enforced` | `dev` disables auth (not for exposed deploys) |
| `--build` / `-Build` | off | build from local source instead of the public image |
| `--private` / `-Private` | off (public) | require IAM auth on the URL instead of app-level keys |
| `--min-instances` / `-MinInstances` | `0` | `0` = scale to zero (free when idle) |
| `--max-instances` / `-MaxInstances` | `2` | cap to avoid surprise scale-out |

## After deploy — wire an app
```bash
URL=https://lighttrack-xxxxx-uc.a.run.app
ADMIN=...                                   # printed by the deploy script
# 1) create a project and an ingest key:
curl -s -X POST $URL/v1/projects -H "Authorization: Bearer $ADMIN" \
  -H 'content-type: application/json' -d '{"name":"demo"}'
curl -s -X POST $URL/v1/projects/<project_id>/keys -H "Authorization: Bearer $ADMIN"
# 2) point a client SDK at it (clients/{python,typescript,rust}):
#    LIGHTTRACK_BASE=$URL  LIGHTTRACK_API_KEY=lt_...   (see clients/ READMEs)
```
The scoring/benchmark **runner** is not deployed here — run `lt-runner serve` on demand from your
laptop (or a tiny VM) pointed at `--base $URL`. It only needs to run when you actually score/benchmark.

## Production checklist
- Keep `--auth-mode enforced` and rotate the admin key (`gcloud secrets versions add`).
- Use a durable `--database-url` (Neon/Supabase/Cloud SQL).
- TLS is handled by Cloud Run automatically.
- Set a billing budget alert so an unexpected spike can't surprise you.

## Teardown
```bash
gcloud run services delete lighttrack --region us-central1
gcloud secrets delete lighttrack-admin-key
gcloud secrets delete lighttrack-database-url   # if created
```
