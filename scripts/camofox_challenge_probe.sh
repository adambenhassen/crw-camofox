#!/usr/bin/env bash
# Manual spike tool — NOT used by the server.
# Drives a local camofox-browser against a Cloudflare-challenged URL and prints,
# every 2 s, the page title, whether the managed-challenge DOM ids are present,
# and the current URL. Then dumps cookies. Optionally clicks a selector and
# repeats the probe.
#
# Usage: scripts/camofox_challenge_probe.sh <url> [click-selector]
# Env:   CAMOFOX_URL (default http://127.0.0.1:9377)
set -euo pipefail

BASE="${CAMOFOX_URL:-http://127.0.0.1:9377}"
URL="${1:?usage: $0 <url> [click-selector]}"
SEL="${2:-}"
USER="crw-probe"
JSON='content-type: application/json'

tab=$(curl -sS -X POST "$BASE/tabs" -H "$JSON" \
  -d "{\"userId\":\"$USER\",\"sessionKey\":\"probe\"}" | jq -r .tabId)
echo "tab=$tab"

# navigate may answer 500 on a huge challenge page (its ARIA snapshot times
# out) even though the navigation committed — keep going.
curl -sS -X POST "$BASE/tabs/$tab/navigate" -H "$JSON" \
  -d "{\"userId\":\"$USER\",\"url\":\"$URL\"}" >/dev/null || true

probe() {
  curl -sS -X POST "$BASE/tabs/$tab/evaluate" -H "$JSON" \
    -d "{\"userId\":\"$USER\",\"expression\":\"JSON.stringify({t:document.title,c:!!document.querySelector('#challenge-running,#challenge-form,#challenge-error-text,#challenge-stage'),u:location.href})\"}" \
    | jq -r '.result // .error'
}

cookies() { curl -sS "$BASE/tabs/$tab/cookies?userId=$USER"; }

for i in $(seq 1 20); do
  echo "t+$((i * 2))s $(probe)"
  sleep 2
done
echo "cookies: $(cookies)"

if [ -n "$SEL" ]; then
  echo "click $SEL: $(curl -sS -X POST "$BASE/tabs/$tab/click" -H "$JSON" \
    -d "{\"userId\":\"$USER\",\"selector\":\"$SEL\"}")"
  for i in $(seq 1 5); do
    echo "post-click t+$((i * 2))s $(probe)"
    sleep 2
  done
  echo "cookies: $(cookies)"
fi

curl -sS -X DELETE "$BASE/tabs/$tab" -H "$JSON" -d "{\"userId\":\"$USER\"}" >/dev/null
