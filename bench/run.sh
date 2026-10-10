#!/usr/bin/env bash
# Query load benchmark.
#
# Usage: bench/run.sh DATA_DIR [DURATION] [CONNECTIONS]
#   env: GEORS (binary, default target/release/geors), PORT (default 2399),
#        EVERY (export sampling, default 20), MIXES (default: all)
#
# Generates query mixes from the data itself (bench/make_queries.py), starts
# a server, runs wrk once per mix and reports throughput, latency
# percentiles, errors and server memory.
set -euo pipefail
DATA=${1:?usage: bench/run.sh DATA_DIR [DURATION] [CONNECTIONS]}
DUR=${2:-20s}
CONN=${3:-32}
PORT=${PORT:-2399}
BIN=${GEORS:-target/release/geors}
EVERY=${EVERY:-20}
MIXES=${MIXES:-full prefix typo bias radius reverse nearest}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
command -v wrk >/dev/null || { echo "wrk not found (brew install wrk / apt install wrk)"; exit 1; }

echo "== generating queries (every ${EVERY}th place)"
for dir in "$DATA"/*/; do
  cc=$(basename "$dir")
  [[ -f "$dir/meta.json" ]] && "$BIN" export --data "$DATA" --every "$EVERY" "$cc" 2>/dev/null || true
done | python3 "$HERE/make_queries.py" "$WORK/queries"

"$BIN" serve --data "$DATA" --bind "127.0.0.1:$PORT" >"$WORK/server.log" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null; rm -rf "$WORK"' EXIT
for _ in $(seq 1 100); do curl -sf "localhost:$PORT/status" >/dev/null && break; sleep 0.2; done
# RSS includes cached index pages (clean, evictable). On macOS, also report
# the physical footprint, which is what Activity Monitor shows as "Memory".
rss() {
  ps -o rss= -p "$PID" | awk '{printf "RSS %.0f MiB", $1/1024}'
  if [[ $(uname) == Darwin ]]; then
    footprint -p "$PID" 2>/dev/null | awk '/phys_footprint:/ {printf ", footprint %s %s", $2, $3; exit}'
  fi
}
echo "== server ready: $(rss)"

printf "%-8s %10s %9s %9s %9s %8s\n" mix req/s p50 p90 p99 errors
for mix in $MIXES; do
  out=$(QUERIES="$WORK/queries/$mix.txt" wrk -t"$(( CONN < 4 ? CONN : 4 ))" -c"$CONN" -d"$DUR" --latency -s "$HERE/wrk.lua" "http://127.0.0.1:$PORT")
  rps=$(awk '/Requests\/sec/ {print $2}' <<<"$out")
  p50=$(awk '$1=="50%" {print $2}' <<<"$out")
  p90=$(awk '$1=="90%" {print $2}' <<<"$out")
  p99=$(awk '$1=="99%" {print $2}' <<<"$out")
  err=$(awk '/Non-2xx/ {print $NF}' <<<"$out")
  printf "%-8s %10s %9s %9s %9s %8s\n" "$mix" "$rps" "$p50" "$p90" "$p99" "${err:-0}"
done
echo "== server after load: $(rss)"
