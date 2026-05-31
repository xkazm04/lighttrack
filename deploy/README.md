# LightTrack — deploy

Packaging & deployment assets. Design: [`docs/PACKAGING.md`](../docs/PACKAGING.md). Guided setup:
run `/onboard` in Claude Code.

## What's here
| Path | Purpose | Status |
|---|---|---|
| `docker/Dockerfile` | One image, all binaries (api/runner/mcp/cli); musl/distroless later | 5b |
| `compose/docker-compose.yml` | Local stack (api + SQLite volume; Postgres/Grafana stubs) | 5b |
| `../.github/workflows/docker.yml` | Multi-arch build → GHCR on tags / manual | 5b |
| `terraform/` | Per-cloud modules (aws/gcp/azure) | planned 5d |
| `helm/` | Kubernetes chart | planned 5e |

## Quick start (local, Docker)
```bash
# from repo root, with Docker running:
docker build -f deploy/docker/Dockerfile -t lighttrack .
docker run -p 8787:8787 -v lt-data:/data lighttrack
curl localhost:8787/health        # -> ok

# or the whole stack:
cd deploy/compose && docker compose up --build
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
- Postgres backend (`LIGHTTRACK_DATABASE_URL`) lands in 5a; until then the image uses SQLite on `/data`.
