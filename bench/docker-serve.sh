#!/usr/bin/env bash
# Serve an existing data directory in Docker under a memory and CPU limit,
# load-test it, and write a stats file.
#
# Usage (from the repository root, Docker Desktop running):
#   bench/docker-serve.sh [DATA_DIR] [MEMORY] [CPUS]
#   bench/docker-serve.sh data-europe 512m 1        # the defaults
#   bench/docker-serve.sh data-europe 256m 1
#
# Settings (environment variables, all optional):
#   DURATION      wrk duration per query mix (default 20s)
#   CONNECTIONS   concurrent connections (default 8)
#   EVERY         sample every Nth place to build queries (default 500)
#   MIXES         query mixes (default: full prefix typo bias radius reverse nearest)
#   SKIP_BUILD=1  use the existing geors:latest image
#   RECOPY=1      copy the data into the Docker volume again
#
# The data is copied once into a Docker volume (geors-serve-<dir name>) and
# reused by later runs until the data directory's GENERATION changes. On
# macOS, bind mounts go through a slow file-sharing layer that makes every
# page-cache miss expensive; a volume behaves like a server's local disk.
# Remove the volume afterwards with: docker volume rm geors-serve-<dir name>
#
# Needs: docker, wrk, python3. Result: bench-results/docker-serve-<dir>-<mem>-<cpus>cpu.txt

set -euo pipefail
cd "$(dirname "$0")/.."

DATA=${1:-data-europe}
MEM=${2:-512m}
CPUS=${3:-1}
DURATION=${DURATION:-20s}
CONNECTIONS=${CONNECTIONS:-8}
EVERY=${EVERY:-500}
MIXES=${MIXES:-full prefix typo bias radius reverse nearest}
IMAGE=geors:latest
NAME=geors-serve-test
PORT=2399
VOLUME=geors-serve-$(basename "$DATA")
THREADS=$(awk -v c="$CPUS" 'BEGIN {t = int(c); print (t < 1 ? 1 : t)}')
WRK_THREADS=$(( CONNECTIONS < 2 ? CONNECTIONS : 2 ))

for tool in docker wrk python3; do
  command -v $tool >/dev/null || { echo "missing: $tool"; exit 1; }
done
docker info >/dev/null 2>&1 || { echo "Docker is not running: start Docker Desktop first"; exit 1; }
[[ -f "$DATA/GENERATION" ]] || { echo "$DATA is not a geors data directory (no GENERATION file)"; exit 1; }
PARTITIONS=$(for d in "$DATA"/*/; do [[ -f "$d/meta.json" ]] && basename "$d"; done | tr '\n' ' ')
[[ -n "$PARTITIONS" ]] || { echo "no partitions in $DATA"; exit 1; }

mkdir -p bench-results
OUT="bench-results/docker-serve-$(basename "$DATA")-$MEM-${CPUS}cpu.txt"
WORK=$(mktemp -d)
cleanup() {
  docker rm -f $NAME >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# ---------------------------------------------------------------- image
if [[ "${SKIP_BUILD:-0}" != 1 ]]; then
  echo "building image (a few minutes the first time)..."
  docker build -q -t $IMAGE . >/dev/null
fi

# ---------------------------------------------------------------- data
generation=$(cat "$DATA/GENERATION")
copied=$(docker run --rm -v "$VOLUME:/vol" alpine cat /vol/.copied-generation 2>/dev/null || true)
if [[ "${RECOPY:-0}" == 1 || "$copied" != "$generation" ]]; then
  need_kb=$(du -sk "$DATA" | cut -f1)
  free_kb=$(docker run --rm alpine df -k / | awk 'NR == 2 {print $4}')
  if (( need_kb > free_kb )); then
    echo "not enough space in Docker's disk: need $((need_kb / 1048576)) GB, $((free_kb / 1048576)) GB free"
    echo "(Docker Desktop > Settings > Resources > Disk usage limit)"
    exit 1
  fi
  echo "copying $(du -sh "$DATA" | cut -f1) into volume $VOLUME (once; reused by later runs)..."
  docker volume rm -f "$VOLUME" >/dev/null 2>&1 || true
  docker volume create "$VOLUME" >/dev/null
  copy="cp /src/GENERATION /vol/"
  for p in $PARTITIONS; do copy="$copy && cp -r /src/$p /vol/"; done
  docker run --rm -v "$VOLUME:/vol" -v "$PWD/$DATA:/src:ro" alpine \
    sh -c "$copy && echo '$generation' > /vol/.copied-generation && chown -R 65532:65532 /vol"
else
  echo "reusing volume $VOLUME (same data generation)"
fi

# ---------------------------------------------------------------- queries
echo "generating queries (every ${EVERY}th place)..."
if [[ -x target/release/geors ]]; then
  export_cmd() { target/release/geors export --data "$DATA" --every "$EVERY" "$1"; }
else
  export_cmd() { docker run --rm -v "$VOLUME:/data:ro" $IMAGE export --data /data --every "$EVERY" "$1"; }
fi
for p in $PARTITIONS; do export_cmd "$p" 2>/dev/null || true; done \
  | python3 bench/make_queries.py "$WORK/q" >/dev/null

# ---------------------------------------------------------------- serve
mem_now() {
  docker stats --no-stream --format '{{.MemUsage}}' $NAME 2>/dev/null | cut -d/ -f1 | tr -d ' '
}
docker rm -f $NAME >/dev/null 2>&1 || true
start=$(date +%s)
docker run -d --name $NAME --memory "$MEM" --memory-swap "$MEM" --cpus "$CPUS" \
  -e GEORS_THREADS="$THREADS" -v "$VOLUME:/data:ro" -p $PORT:2322 \
  $IMAGE serve --data /data >/dev/null
ready=0
for _ in $(seq 1 600); do
  curl -sf "localhost:$PORT/status" >/dev/null && { ready=1; break; }
  [[ "$(docker inspect -f '{{.State.Running}}' $NAME)" == true ]] || break
  sleep 0.5
done
if [[ $ready != 1 ]]; then
  echo "server did not start (OOM killed: $(docker inspect -f '{{.State.OOMKilled}}' $NAME 2>/dev/null)):"
  docker logs $NAME 2>&1 | tail -5
  exit 1
fi
startup=$(( $(date +%s) - start ))

{
  echo "geors serving in Docker  $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "data: $DATA ($(du -sh "$DATA" | cut -f1), $(wc -w <<<"$PARTITIONS" | tr -d ' ') partitions)"
  echo "limits: --memory $MEM --cpus $CPUS (GEORS_THREADS=$THREADS)"
  echo "docker host: $(docker info --format '{{.NCPU}} CPUs, {{.MemTotal}} bytes RAM, {{.OperatingSystem}}')"
  echo "load: wrk, $CONNECTIONS connections, $DURATION per mix, queries from every ${EVERY}th place"
  echo
  echo "startup: ${startup}s   memory at idle: $(mem_now)"
  echo
  printf "%-8s %10s %10s %10s %10s %8s %10s\n" mix req/s p50 p90 p99 errors memory
} | tee "$OUT"

for mix in $MIXES; do
  res=$(QUERIES="$WORK/q/$mix.txt" wrk -t"$WRK_THREADS" -c"$CONNECTIONS" -d"$DURATION" --latency \
    -s bench/wrk.lua "http://127.0.0.1:$PORT" 2>&1 || true)
  rps=$(awk '/Requests\/sec/ {printf "%.0f", $2}' <<<"$res")
  p50=$(awk '$1 == "50%" {print $2}' <<<"$res")
  p90=$(awk '$1 == "90%" {print $2}' <<<"$res")
  p99=$(awk '$1 == "99%" {print $2}' <<<"$res")
  err=$(awk '/Non-2xx/ {n += $NF} /Socket errors/ {gsub(",", ""); n += $4 + $6 + $8 + $10} END {print n + 0}' <<<"$res")
  if [[ "$(docker inspect -f '{{.State.Running}}' $NAME)" != true ]]; then
    echo "server died during '$mix' (OOM killed: $(docker inspect -f '{{.State.OOMKilled}}' $NAME))" | tee -a "$OUT"
    exit 1
  fi
  printf "%-8s %10s %10s %10s %10s %8s %10s\n" "$mix" "${rps:-0}" "${p50:--}" "${p90:--}" "${p99:--}" "$err" "$(mem_now)" | tee -a "$OUT"
done

{
  echo
  echo "OOM killed: $(docker inspect -f '{{.State.OOMKilled}}' $NAME)"
  echo "memory = container usage excluding reclaimable page cache (docker stats)"
} | tee -a "$OUT"
echo "done: $OUT   (volume $VOLUME kept for the next run)"
