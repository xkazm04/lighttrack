#!/usr/bin/env sh
# LightTrack smoke test — drives the real core loop against a RUNNING instance.
#
#   scripts/smoke.sh http://127.0.0.1:8787
#
# The point is to exercise a *built artifact* (a container image, a release binary), not the source
# tree: health -> create project -> mint a project API key -> ingest an event with that key -> read
# the event back and assert the cost was actually priced from the book.
#
# Env:
#   LIGHTTRACK_ADMIN_KEY  required — the instance's admin key (project/key creation is admin-only).
#   SMOKE_WAIT_SECS       how long to wait for /health to come up (default 60; 0 = no wait).
#   SMOKE_TIMEOUT_SECS    per-request timeout (default 15).
#   SMOKE_MODEL/_PROVIDER a priced (provider, model) pair from config/pricing.json (default
#                         openai/gpt-4o-mini). Must be in the price book or the cost assertion fails,
#                         which is the point: an unpriced model means cost stayed null.
#
# Works against an instance in either auth mode: an admin key is accepted before the dev-mode
# fallback, so this never leans on dev-mode implicit behaviour. Depends only on curl + POSIX text
# tools (no jq), same as deploy/install.sh.
set -eu

BASE="${1:-}"
[ -n "$BASE" ] || { echo "usage: $0 <base-url>   (e.g. $0 http://127.0.0.1:8787)" >&2; exit 2; }
BASE="${BASE%/}"

ADMIN="${LIGHTTRACK_ADMIN_KEY:-}"
[ -n "$ADMIN" ] || { echo "LIGHTTRACK_ADMIN_KEY must be set (admin-only endpoints are used)" >&2; exit 2; }

WAIT_SECS="${SMOKE_WAIT_SECS:-60}"
TIMEOUT="${SMOKE_TIMEOUT_SECS:-15}"
PROVIDER="${SMOKE_PROVIDER:-openai}"
MODEL="${SMOKE_MODEL:-gpt-4o-mini}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
BODY="$tmp/body"
ERRF="$tmp/err"
STATUS=""

step() { echo "smoke: $*"; }
fail() { echo "SMOKE FAILED: $*" >&2; exit 1; }

# Body excerpt for failure messages — never let a step die without saying what the server said.
excerpt() { head -c 400 "$BODY" 2>/dev/null || true; }

# http METHOD PATH TOKEN [JSON_BODY] -> sets $STATUS, writes the response to $BODY.
http() {
  _m="$1"; _p="$2"; _t="$3"; _b="${4:-}"
  : >"$BODY"
  set -- -sS --max-time "$TIMEOUT" -o "$BODY" -w '%{http_code}' \
         -X "$_m" -H "Authorization: Bearer $_t"
  [ -z "$_b" ] || set -- "$@" -H 'Content-Type: application/json' -d "$_b"
  STATUS="$(curl "$@" "$BASE$_p" 2>"$ERRF")" || {
    _rc=$?
    fail "$_m $_p: curl exited $_rc — $(tr -d '\r' <"$ERRF" | tr '\n' ' ')"
  }
}

# expect STATUS_CODE WHAT
expect() {
  [ "$STATUS" = "$1" ] || fail "$2: expected HTTP $1, got ${STATUS:-<none>}; body: $(excerpt)"
}

# json_str KEY — first "key": "value" in the response body.
json_str() {
  sed -n 's/.*"'"$1"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$BODY" | head -n 1
}

# json_num KEY — first "key": <number> in the response body.
json_num() {
  sed -n 's/.*"'"$1"'"[[:space:]]*:[[:space:]]*\(-\{0,1\}[0-9][0-9.eE+-]*\).*/\1/p' "$BODY" | head -n 1
}

# ---- 1. health -----------------------------------------------------------
# Poll rather than assume: a freshly started container needs a moment to bind.
step "waiting up to ${WAIT_SECS}s for $BASE/health"
waited=0
while : ; do
  if curl -fsS --max-time "$TIMEOUT" -o "$BODY" "$BASE/health" 2>"$ERRF"; then
    break
  fi
  [ "$waited" -lt "$WAIT_SECS" ] || fail "/health never became ready after ${WAIT_SECS}s — $(tr -d '\r' <"$ERRF" | tr '\n' ' ')"
  sleep 1
  waited=$((waited + 1))
done
health="$(tr -d '\r\n' <"$BODY")"
[ "$health" = "ok" ] || fail "/health returned '$health', expected 'ok'"
step "health ok (after ${waited}s)"

# ---- 2. create a project (admin) -----------------------------------------
name="smoke-$(date -u +%Y%m%d%H%M%S)-$$"
http POST /v1/projects "$ADMIN" "{\"name\":\"$name\"}"
expect 200 "create project"
pid="$(json_str id)"
[ -n "$pid" ] || fail "create project: no id in response; body: $(excerpt)"
step "created project $pid"

# ---- 3. mint a project API key (admin) -----------------------------------
http POST "/v1/projects/$pid/keys" "$ADMIN" '{"name":"smoke"}'
expect 200 "create api key"
key="$(json_str key)"
[ -n "$key" ] || fail "create api key: no key in response; body: $(excerpt)"
step "minted project key ${key%%_*}_… "

# ---- 4. ingest an event with the PROJECT key -----------------------------
# No project_id in the body on purpose: the key must scope the event to its own project. That is the
# keyed-ingest contract, and it does not depend on any dev-mode fallback.
event="{\"provider\":\"$PROVIDER\",\"model\":\"$MODEL\",\"operation\":\"chat\",\"status\":\"success\",\"name\":\"smoke\",\"usage\":{\"input\":1000,\"output\":500}}"
http POST /v1/events "$key" "$event"
expect 200 "ingest event"
eid="$(json_str id)"
[ -n "$eid" ] || fail "ingest: no event id in response; body: $(excerpt)"
step "ingested event $eid"

# ---- 5. read it back and assert the cost was priced ----------------------
http GET "/v1/events/$eid" "$key"
expect 200 "read event back"

got_pid="$(json_str project_id)"
[ "$got_pid" = "$pid" ] || fail "read back: event project_id '$got_pid' != '$pid' (key scoping is wrong)"

cost="$(json_num cost_usd)"
[ -n "$cost" ] || fail "read back: cost_usd is absent/null — $PROVIDER/$MODEL was not priced. body: $(excerpt)"
awk -v c="$cost" 'BEGIN { exit !(c + 0 > 0) }' \
  || fail "read back: cost_usd is '$cost', expected a positive number (a phantom zero is not a price)"

# `cost_source` proves the number came from the DB price book rather than being echoed back from the
# client — the invariant that makes cost dashboards trustworthy.
src="$(json_str cost_source)"
[ "$src" = "book" ] || fail "read back: cost_source is '${src:-<absent>}', expected 'book'"
step "cost_usd=$cost priced from the book"

echo "SMOKE PASSED: $BASE (project $pid, event $eid, cost \$$cost)"
