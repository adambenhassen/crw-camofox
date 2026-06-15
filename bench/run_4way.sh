#!/usr/bin/env bash
# Sequential 4-way renderer comparison: none / lightpanda / browserless / auto.
# Each pass: restart crw with a different CRW_RENDERER__MODE, run 1000-URL bench,
# capture JSON + log.
set -euo pipefail

cd "$(dirname "$0")/.."

# Load .env (HF_TOKEN, BROWSERLESS_TOKEN) so HuggingFace stops nagging.
if [ -f .env ]; then
  set -a; . ./.env; set +a
fi

TS=$(date -u +%Y%m%d-%H%M%S)
OUT=bench/server-runs
mkdir -p "$OUT"

URLS="${BENCH_MAX_URLS:-1000}"
CONC="${BENCH_CONCURRENCY:-10}"
PORT="${CRW_PORT:-3030}"
COMPOSE_FILES=(-f docker-compose.yml -f docker-compose.override.yml)

run_pass() {
  local mode="$1"
  local label="$2"
  echo
  echo "============================================================"
  echo "PASS: $label  (CRW_RENDERER__MODE=$mode, urls=$URLS, conc=$CONC)"
  echo "============================================================"

  CRW_RENDERER__MODE="$mode" docker compose "${COMPOSE_FILES[@]}" \
    up -d --force-recreate crw

  for i in {1..60}; do
    if curl -sf "http://localhost:$PORT/health" >/dev/null 2>&1; then
      echo "crw ready (port $PORT, mode=$mode)"
      break
    fi
    sleep 2
  done

  json="$OUT/4way-$TS-$label.json"
  log="$OUT/4way-$TS-$label.log"

  CRW_API_URL="http://localhost:$PORT" \
  BENCH_CONCURRENCY="$CONC" \
  BENCH_MAX_URLS="$URLS" \
  BENCH_RESULTS_PATH="$json" \
  bench/.venv/bin/python bench/run_bench.py 2>&1 | tee "$log"

  echo "→ saved $json"
}

run_pass none       none
run_pass lightpanda lightpanda
run_pass chrome     browserless
run_pass auto       auto

echo
echo "============================================================"
echo "ALL DONE. Artifacts:"
ls -1 "$OUT"/4way-"$TS"-*.json
