#!/usr/bin/env bash
# Benchmark geors in Docker under memory / CPU limits.
#
# Usage (from the repository root):
#   bench/docker-bench.sh
#
# Settings (environment variables, all optional):
#   DATA_DIR        partitions to serve (default: data)
#   PBF             extract for the import tests (default: none = skip imports)
#   SERVE_LIMITS    "MEMORY:CPUS ..." for serving (default: "4g:4 4g:1 1g:1 256m:1 100m:1")
#   IMPORT_LIMITS   "MEMORY:CPUS ..." for imports (default: "4g:2 1g:1 512m:1 256m:1")
#   DURATION        wrk duration per query mix (default: 10s)
#   CONNECTIONS     concurrent connections (default: 4)
#   IMPORT_TIMEOUT  seconds before an import test is stopped (default: 3600)
#   EVERY           sample every Nth place to build queries (default: 50)
#   SKIP_BUILD=1    use the existing geors:latest image
#   KEEP_VOLUME=1   keep the geors-bench volume afterwards
#
# Needs: docker, wrk, python3. Result: bench-results/docker-<timestamp>.txt
#
# Data is copied into a Docker volume first. On macOS, bind mounts go through
# a slow file-sharing layer that makes every page-cache miss expensive; a
# volume behaves like a local disk on a Linux server.

set -euo pipefail
cd "$(dirname "$0")/.."

DATA_DIR=${DATA_DIR:-data}
PBF=${PBF:-}
SERVE_LIMITS=${SERVE_LIMITS:-"4g:4 4g:1 1g:1 256m:1 100m:1"}
IMPORT_LIMITS=${IMPORT_LIMITS:-"4g:2 1g:1 512m:1 256m:1"}
DURATION=${DURATION:-10s}
CONNECTIONS=${CONNECTIONS:-4}
IMPORT_TIMEOUT=${IMPORT_TIMEOUT:-3600}
EVERY=${EVERY:-50}
IMAGE=geors:latest
VOLUME=geors-bench
PORT=2399
MIXES="full prefix typo bias radius reverse nearest"

for tool in docker wrk python3; do
  command -v $tool >/dev/null || { echo "missing: $tool"; exit 1; }
done
docker info >/dev/null 2>&1 || { echo "docker daemon not running"; exit 1; }

mkdir -p bench-results
OUT="bench-results/docker-$(date +%Y%m%d-%H%M%S).txt"
WORK=$(mktemp -d)
cleanup() {
  docker rm -f geors-bench-serve geors-bench-import >/dev/null 2>&1 || true
  [[ "${KEEP_VOLUME:-0}" == 1 ]] || docker volume rm -f $VOLUME >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

log() { echo "$*" | tee -a "$OUT"; }

# ---------------------------------------------------------------- setup
if [[ "${SKIP_BUILD:-0}" != 1 ]]; then
  echo "building image..."
  docker build -q -t $IMAGE . >/dev/null
fi

echo "copying data into volume $VOLUME..."
docker volume rm -f $VOLUME >/dev/null 2>&1 || true
docker volume create $VOLUME >/dev/null
PARTITIONS=$(for d in "$DATA_DIR"/*/; do [[ -f "$d/meta.json" ]] && basename "$d"; done | tr '\n' ' ')
[[ -n "$PARTITIONS" ]] || { echo "no partitions in $DATA_DIR"; exit 1; }
copy="mkdir -p /vol/data /vol/osm"
for p in $PARTITIONS; do copy="$copy && cp -r /src/$p /vol/data/"; done
mounts=(-v "$VOLUME:/vol" -v "$PWD/$DATA_DIR:/src:ro")
if [[ -n "$PBF" ]]; then
  mounts+=(-v "$(cd "$(dirname "$PBF")" && pwd)/$(basename "$PBF"):/in.osm.pbf:ro")
  copy="$copy && cp /in.osm.pbf /vol/osm/input.osm.pbf"
fi
docker run --rm "${mounts[@]}" alpine sh -c "$copy && chown -R 65532:65532 /vol"

echo "generating queries..."
for p in $PARTITIONS; do
  docker run --rm -v $VOLUME:/vol $IMAGE export --data /vol/data --every "$EVERY" "$p" 2>/dev/null
done | python3 bench/make_queries.py "$WORK/q" >/dev/null

HOST=$(docker info --format '{{.NCPU}} CPUs, {{.MemTotal}} bytes RAM, {{.OperatingSystem}}')
{
  echo "geors Docker benchmark  $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "docker host: $HOST"
  echo "image: $(docker images $IMAGE --format '{{.Size}}')   partitions: $PARTITIONS"
  echo "index size: $(docker run --rm -v $VOLUME:/vol alpine du -sh /vol/data | cut -f1)"
  echo "load: wrk, $CONNECTIONS connections, $DURATION per mix; queries sampled from the data"
  echo
} > "$OUT"

mem_now() { docker stats --no-stream --format '{{.MemUsage}}' "$1" | cut -d/ -f1 | tr -d ' '; }

# ---------------------------------------------------------------- serving
log "== serving (req/s, p50, p99)"
header=$(printf "%-12s %-6s" limit idle; for m in $MIXES; do printf " %-22s" "$m"; done; printf " %-8s %s" after OOM)
log "$header"
for spec in $SERVE_LIMITS; do
  mem=${spec%%:*}; cpus=${spec##*:}
  docker rm -f geors-bench-serve >/dev/null 2>&1 || true
  docker run -d --name geors-bench-serve --memory "$mem" --memory-swap "$mem" --cpus "$cpus" \
    -e GEORS_THREADS="${cpus%.*}" -v $VOLUME:/vol:ro -p $PORT:2322 \
    $IMAGE serve --data /vol/data >/dev/null
  ready=0
  for _ in $(seq 1 150); do curl -sf "localhost:$PORT/status" >/dev/null && { ready=1; break; }; sleep 0.2; done
  if [[ $ready != 1 ]]; then
    log "$(printf "%-12s did not start: %s" "$mem/$cpus" "$(docker logs geors-bench-serve 2>&1 | tail -1)")"
    continue
  fi
  idle=$(mem_now geors-bench-serve)
  row=$(printf "%-12s %-6s" "$mem/${cpus}cpu" "$idle")
  for mix in $MIXES; do
    res=$(QUERIES="$WORK/q/$mix.txt" wrk -t2 -c"$CONNECTIONS" -d"$DURATION" --latency \
      -s bench/wrk.lua "http://127.0.0.1:$PORT" 2>&1 || true)
    rps=$(awk '/Requests\/sec/ {printf "%.0f", $2}' <<<"$res")
    p50=$(awk '$1=="50%" {print $2}' <<<"$res")
    p99=$(awk '$1=="99%" {print $2}' <<<"$res")
    err=$(awk '/Non-2xx|timeout [1-9]/ {e=1} END {if (e) printf "!"}' <<<"$res")
    row="$row $(printf "%-22s" "${rps:-0} ${p50:--} ${p99:--}$err")"
  done
  after=$(mem_now geors-bench-serve)
  oom=$(docker inspect -f '{{.State.OOMKilled}}' geors-bench-serve)
  log "$row $(printf "%-8s %s" "$after" "$oom")"
  docker rm -f geors-bench-serve >/dev/null
done
log "('!' = errors or timeouts in that mix; memory is the container's usage excluding reclaimable page cache)"
log

# ---------------------------------------------------------------- import
if [[ -n "$PBF" ]]; then
  log "== import of $(basename "$PBF") ($(du -h "$PBF" | cut -f1))"
  log "$(printf "%-12s %-10s %-10s %-10s %-6s %s" limit result time peak_mem OOM phases)"
  for spec in $IMPORT_LIMITS; do
    mem=${spec%%:*}; cpus=${spec##*:}
    docker rm -f geors-bench-import >/dev/null 2>&1 || true
    docker run --rm -v $VOLUME:/vol alpine rm -rf /vol/import-test
    start=$(date +%s)
    docker run -d --name geors-bench-import --memory "$mem" --memory-swap "$mem" --cpus "$cpus" \
      -e GEORS_THREADS="${cpus%.*}" -v $VOLUME:/vol \
      $IMAGE import /vol/osm/input.osm.pbf --data /vol/import-test --no-track >/dev/null
    peak=0; result=ok
    while [[ "$(docker inspect -f '{{.State.Running}}' geors-bench-import)" == true ]]; do
      m=$(docker stats --no-stream --format '{{.MemUsage}}' geors-bench-import | cut -d/ -f1 \
        | awk '{v=$1+0; if ($1 ~ /GiB/) v*=1024; if ($1 ~ /KiB/) v/=1024; printf "%d", v}')
      (( m > peak )) && peak=$m
      if (( $(date +%s) - start > IMPORT_TIMEOUT )); then
        docker kill geors-bench-import >/dev/null; result=timeout; break
      fi
      sleep 5
    done
    secs=$(( $(date +%s) - start ))
    code=$(docker inspect -f '{{.State.ExitCode}}' geors-bench-import)
    oom=$(docker inspect -f '{{.State.OOMKilled}}' geors-bench-import)
    [[ $result == ok && $code != 0 ]] && result="exit $code"
    phases=$(docker logs geors-bench-import 2>&1 | sed 's/\x1b\[[0-9;]*m//g' \
      | grep -E "pass [1-5]/5|wrote partition" \
      | sed -E 's/.*pass ([1-5])\/5.*elapsed=([0-9.]+)([a-z]+).*/p\1=\2\3/; s/.*wrote partition.*elapsed=([0-9.]+)([a-z]+).*/write=\1\2/' \
      | awk '{printf "%s ", $0}')
    log "$(printf "%-12s %-10s %-10s %-10s %-6s %s" "$mem/${cpus}cpu" "$result" "${secs}s" "${peak}MiB" "$oom" "$phases")"
    docker rm -f geors-bench-import >/dev/null
  done
  log
fi

log "done: $OUT"
