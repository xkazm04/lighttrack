#!/usr/bin/env bash
# LightTrack → Google Cloud Run, one command.
#
# Deploys the API as a scale-to-zero Cloud Run service. Free-tier friendly: with --min-instances 0
# you pay nothing while idle, and Cloud Run's always-free quota (2M req, 180k vCPU-s, 360k GiB-s /mo)
# covers low-traffic test apps.
#
#   ./deploy.sh --project my-gcp-project [--region us-central1] [--database-url postgres://...]
#
# Defaults to the published public image (ghcr.io/xkazm04/tracklight). Use --build to build the
# image from local source via Cloud Build (for forks / local changes).
#
# Storage:
#   - No --database-url  => ephemeral SQLite on the container disk. Fine for a smoke test, but data
#                           is LOST on every cold start / new revision. Do NOT use for real data.
#   - --database-url ... => durable Postgres (e.g. a free Neon DSN). Recommended for anything real.
#
# Auth: defaults to enforced + a generated admin key (printed once). Ingress is public so apps can
# POST events with their project API keys; /health stays open; /v1 management needs the admin key.
set -euo pipefail

# --- defaults ---------------------------------------------------------------
PROJECT=""
REGION="us-central1"
SERVICE="lighttrack"
IMAGE="ghcr.io/xkazm04/tracklight:v0.0.2"
BUILD=0
DATABASE_URL=""
ADMIN_KEY=""
AUTH_MODE="enforced"
PUBLIC=1
MIN_INSTANCES=0
MAX_INSTANCES=2
CPU="1"
MEMORY="256Mi"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

usage() { sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --project)        PROJECT="$2"; shift 2;;
    --region)         REGION="$2"; shift 2;;
    --service)        SERVICE="$2"; shift 2;;
    --image)          IMAGE="$2"; shift 2;;
    --build)          BUILD=1; shift;;
    --database-url)   DATABASE_URL="$2"; shift 2;;
    --admin-key)      ADMIN_KEY="$2"; shift 2;;
    --auth-mode)      AUTH_MODE="$2"; shift 2;;
    --private)        PUBLIC=0; shift;;
    --min-instances)  MIN_INSTANCES="$2"; shift 2;;
    --max-instances)  MAX_INSTANCES="$2"; shift 2;;
    --cpu)            CPU="$2"; shift 2;;
    --memory)         MEMORY="$2"; shift 2;;
    -h|--help)        usage 0;;
    *) echo "unknown arg: $1" >&2; usage 1;;
  esac
done

[[ -z "$PROJECT" ]] && PROJECT="$(gcloud config get-value project 2>/dev/null || true)"
[[ -z "$PROJECT" || "$PROJECT" == "(unset)" ]] && { echo "ERROR: --project is required (or set 'gcloud config set project')." >&2; exit 1; }

echo ">> project=$PROJECT region=$REGION service=$SERVICE auth=$AUTH_MODE"

# --- 1. APIs ----------------------------------------------------------------
echo ">> enabling APIs..."
# Artifact Registry is always needed: Cloud Run can only pull from Artifact Registry / gcr.io,
# so even a prebuilt public image is mirrored into the project's registry first.
APIS="run.googleapis.com secretmanager.googleapis.com artifactregistry.googleapis.com"
[[ $BUILD -eq 1 ]] && APIS="$APIS cloudbuild.googleapis.com"
gcloud services enable $APIS --project "$PROJECT" --quiet

ensure_repo() {
  gcloud artifacts repositories describe "$SERVICE" --location "$REGION" --project "$PROJECT" >/dev/null 2>&1 \
    || gcloud artifacts repositories create "$SERVICE" --repository-format=docker --location "$REGION" --project "$PROJECT" --quiet
}

# --- 2. resolve a deployable image (Artifact Registry) ----------------------
if [[ $BUILD -eq 1 ]]; then
  echo ">> building image from source via Cloud Build (this takes a while)..."
  ensure_repo
  IMAGE="${REGION}-docker.pkg.dev/${PROJECT}/${SERVICE}/${SERVICE}:latest"
  ( cd "$ROOT" && gcloud builds submit --project "$PROJECT" --config deploy/cloudrun/cloudbuild.yaml --substitutions=_IMAGE="$IMAGE" )
else
  case "$IMAGE" in
    *-docker.pkg.dev/*|gcr.io/*|*.gcr.io/*) : ;;   # already deployable by Cloud Run
    *)
      # Cloud Run can't pull from external registries (ghcr.io/docker.io). Front the upstream with an
      # Artifact Registry *remote repository* (a lazy pull-through cache) and deploy that AR path.
      REG_HOST="${IMAGE%%/*}"
      UPSTREAM_PATH="${IMAGE#*/}"
      REMOTE_REPO="${SERVICE}-remote"
      gcloud artifacts repositories describe "$REMOTE_REPO" --location "$REGION" --project "$PROJECT" >/dev/null 2>&1 \
        || gcloud artifacts repositories create "$REMOTE_REPO" --repository-format=docker \
             --mode=remote-repository --remote-docker-repo="https://${REG_HOST}" \
             --location "$REGION" --project "$PROJECT" --quiet
      echo ">> fronting ${REG_HOST} with Artifact Registry remote repo '${REMOTE_REPO}'"
      IMAGE="${REGION}-docker.pkg.dev/${PROJECT}/${REMOTE_REPO}/${UPSTREAM_PATH}"
      ;;
  esac
fi
echo ">> image=$IMAGE"

PROJECT_NUMBER="$(gcloud projects describe "$PROJECT" --format='value(projectNumber)')"
RUNTIME_SA="${PROJECT_NUMBER}-compute@developer.gserviceaccount.com"

# --- 3. secrets -------------------------------------------------------------
# put_secret NAME VALUE  (idempotent: create-if-missing, then add a new version, then grant access)
put_secret() {
  local name="$1" value="$2"
  gcloud secrets describe "$name" --project "$PROJECT" >/dev/null 2>&1 \
    || gcloud secrets create "$name" --replication-policy=automatic --project "$PROJECT" --quiet
  printf '%s' "$value" | gcloud secrets versions add "$name" --data-file=- --project "$PROJECT" --quiet >/dev/null
  gcloud secrets add-iam-policy-binding "$name" --project "$PROJECT" \
    --member="serviceAccount:${RUNTIME_SA}" --role="roles/secretmanager.secretAccessor" --quiet >/dev/null
}

SET_SECRETS=()
if [[ "$AUTH_MODE" == "enforced" ]]; then
  if [[ -z "$ADMIN_KEY" ]]; then
    ADMIN_KEY="$(openssl rand -hex 32 2>/dev/null || head -c 32 /dev/urandom | xxd -p -c 64)"
    GENERATED_KEY=1
  fi
  echo ">> storing admin key in Secret Manager (${SERVICE}-admin-key)"
  put_secret "${SERVICE}-admin-key" "$ADMIN_KEY"
  SET_SECRETS+=("LIGHTTRACK_ADMIN_KEY=${SERVICE}-admin-key:latest")
fi
if [[ -n "$DATABASE_URL" ]]; then
  echo ">> storing database URL in Secret Manager (${SERVICE}-database-url)"
  put_secret "${SERVICE}-database-url" "$DATABASE_URL"
  SET_SECRETS+=("LIGHTTRACK_DATABASE_URL=${SERVICE}-database-url:latest")
else
  echo "!! no --database-url: using EPHEMERAL SQLite (data lost on cold start). Pass a Neon DSN for durable storage."
fi

# --- 4. deploy --------------------------------------------------------------
echo ">> deploying to Cloud Run..."
ARGS=(run deploy "$SERVICE"
  --project "$PROJECT" --region "$REGION" --image "$IMAGE"
  --port 8080
  --set-env-vars "LIGHTTRACK_BIND=0.0.0.0:8080,LIGHTTRACK_AUTH_MODE=${AUTH_MODE}"
  --cpu "$CPU" --memory "$MEMORY"
  --min-instances "$MIN_INSTANCES" --max-instances "$MAX_INSTANCES"
  --quiet)
[[ ${#SET_SECRETS[@]} -gt 0 ]] && ARGS+=(--set-secrets "$(IFS=,; echo "${SET_SECRETS[*]}")")
[[ $PUBLIC -eq 1 ]] && ARGS+=(--allow-unauthenticated) || ARGS+=(--no-allow-unauthenticated)
gcloud "${ARGS[@]}"

URL="$(gcloud run services describe "$SERVICE" --project "$PROJECT" --region "$REGION" --format='value(status.url)')"

# --- 5. health check --------------------------------------------------------
echo ">> health check: ${URL}/health"
if curl -fsS "${URL}/health" >/dev/null 2>&1; then HEALTH="ok"; else HEALTH="UNREACHABLE"; fi

echo
echo "============================================================"
echo " LightTrack deployed: $URL   (health: $HEALTH)"
[[ "${GENERATED_KEY:-0}" -eq 1 ]] && echo " ADMIN KEY (save now, shown once): $ADMIN_KEY"
echo "============================================================"
echo " Next: create a project + ingest key, then point your apps at $URL"
echo "   curl -s -X POST $URL/v1/projects -H 'Authorization: Bearer <ADMIN_KEY>' -H 'content-type: application/json' -d '{\"name\":\"demo\"}'"
