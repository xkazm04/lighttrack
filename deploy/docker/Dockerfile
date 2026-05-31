# syntax=docker/dockerfile:1
#
# LightTrack — one image, all binaries (lighttrack-api, lt-runner, lt-mcp, lt).
# Build from the repo root:  docker build -f deploy/docker/Dockerfile -t lighttrack .
# Multi-arch in CI via buildx (see .github/workflows/docker.yml).

# ---- builder -------------------------------------------------------------
FROM rust:1-bookworm AS builder
WORKDIR /src
# `schema/` is include_str!'d at compile time, so the whole workspace is needed.
# .dockerignore keeps target/.git/etc. out of the context.
COPY . .
RUN cargo build --release --bins

# ---- runtime -------------------------------------------------------------
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd -r -u 10001 -d /app -m lighttrack \
 && mkdir -p /data && chown lighttrack:lighttrack /data
WORKDIR /app

COPY --from=builder /src/target/release/lighttrack-api /usr/local/bin/lighttrack-api
COPY --from=builder /src/target/release/lt-runner      /usr/local/bin/lt-runner
COPY --from=builder /src/target/release/lt-mcp         /usr/local/bin/lt-mcp
COPY --from=builder /src/target/release/lt             /usr/local/bin/lt
COPY config/pricing.json /app/config/pricing.json

ENV LIGHTTRACK_BIND=0.0.0.0:8787 \
    LIGHTTRACK_DB=/data/lighttrack.db \
    LIGHTTRACK_PRICING=/app/config/pricing.json \
    # NOTE: default `dev` for frictionless trial. Set LIGHTTRACK_AUTH_MODE=enforced
    # (and LIGHTTRACK_ADMIN_KEY) for any exposed/cloud deployment.
    LIGHTTRACK_AUTH_MODE=dev

USER lighttrack
EXPOSE 8787
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8787/health || exit 1

# Default to the API. The runner needs the `claude` CLI + provider keys, so it runs
# on the host (or a VM), not from this image — see deploy/README.md.
CMD ["lighttrack-api"]
