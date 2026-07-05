# LightTrack — deploy

Packaging & deployment assets. Design: [`docs/PACKAGING.md`](../docs/PACKAGING.md). Guided setup:
run `/onboard` in Claude Code.

## What's here
| Path | Purpose | Status |
|---|---|---|
| `docker/Dockerfile` | One image, all binaries (api/runner/mcp/cli) + all backends | **available** |
| `compose/docker-compose.yml` | Local stack (api + SQLite volume) | **available** |
| `compose/docker-compose.postgres.yml` | api-on-Postgres + Postgres + Grafana | **available** |
| `cloudrun/deploy.{sh,ps1}` | One-command Cloud Run deploy (scale-to-zero, ~free) | **available** |
| `cloudrun/cloudbuild.yaml` | Build image from source for the `--build` path | **available** |
| `../.github/workflows/docker.yml` | Build → GHCR (public) on `v*` tags / manual | **available** |
| `../.github/workflows/release.yml` | Prebuilt binaries (linux/macOS/Windows) on `v*` tags | **available** |
| `install.sh` / `install.ps1` | One-line binary installers | **available** |
| `terraform/modules/{gcp,azure}` | Cloud Run / Container Apps modules | **available** (AWS planned) |
| `helm/lighttrack` | Kubernetes chart | **available** |

## Quick start (local, Docker)
```bash
# pull the published public image (bundles SQLite/Postgres/Firestore):
docker run -p 8787:8787 -v lt-data:/data ghcr.io/xkazm04/tracklight:v0.0.2
curl localhost:8787/health        # -> ok

# or build from source:
docker build -f deploy/docker/Dockerfile -t lighttrack .
docker run -p 8787:8787 -v lt-data:/data lighttrack

# or the whole local stack (api + SQLite):
cd deploy/compose && docker compose up -d
# with Postgres + Grafana instead:
docker compose -f docker-compose.postgres.yml up -d
```

## The runner
`lt-runner` (the `claude -p` judge + queue worker) runs **on the host / a VM** where `claude` and
provider keys live — not inside the API image. Point it at the API:
```bash
lt-runner --base http://127.0.0.1:8787 serve --interval 10
```

## Production notes
- Set `LIGHTTRACK_AUTH_MODE=enforced` and a strong `LIGHTTRACK_ADMIN_KEY` for any exposed deploy.
- TLS is terminated by the platform (Cloud Run/App Runner/Container Apps) or a reverse proxy; the app
  stays plain HTTP behind it.
- Backend selection is one env var: `LIGHTTRACK_DATABASE_URL=postgres://…` or `firestore://<project>`;
  unset uses SQLite on `/data`. The same image supports all three.
