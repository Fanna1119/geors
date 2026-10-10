#!/usr/bin/env bash
# Import an extract natively and write a stats file.
#
# Usage (from the repository root):
#   bench/import-stats.sh PBF [DATA_DIR] [extra geors import args...]
#
# Example:
#   bench/import-stats.sh ~/Downloads/europe-latest.osm.pbf data-europe --threads 4
#
# Result: bench-results/import-<name>.txt (summary), .log (geors output) and
# .samples (every 15 s: seconds, rss_kb, data_dir_kb, free_disk_kb, swap_mb).
# Works on macOS and Linux.

set -uo pipefail
cd "$(dirname "$0")/.."
PBF=${1:?usage: bench/import-stats.sh PBF [DATA_DIR] [extra import args...]}
DATA=${2:-data}
shift $(( $# >= 2 ? 2 : 1 ))
BIN=${GEORS:-target/release/geors}
[[ -x $BIN ]] || cargo build --release --bin geors

NAME=$(basename "$PBF" | sed -E 's/(-latest)?\.osm\.pbf$//')
mkdir -p bench-results
OUT=bench-results/import-$NAME.txt
LOG=bench-results/import-$NAME.log
SAMPLES=bench-results/import-$NAME.samples
: > "$SAMPLES"

if [[ $(uname) == Darwin ]]; then
  TIME=(/usr/bin/time -l)
  swap_mb() { sysctl -n vm.swapusage | awk '{print $6}' | tr -d 'M'; }
  machine="$(sysctl -n machdep.cpu.brand_string), $(sysctl -n hw.ncpu) cores, $(( $(sysctl -n hw.memsize) / 1073741824 )) GB RAM"
else
  TIME=(/usr/bin/time -v)
  swap_mb() { free -m | awk '/^Swap/ {print $3}'; }
  machine="$(nproc) cores, $(free -g | awk '/^Mem/ {print $2}') GB RAM"
fi

echo "importing $PBF into $DATA (log: $LOG)"
start=$(date +%s)
"${TIME[@]}" "$BIN" import "$PBF" --data "$DATA" "$@" > "$LOG" 2>&1 &
TPID=$!
peak_rss=0; peak_dir=0; min_free=999999999999; peak_swap=0
while kill -0 $TPID 2>/dev/null; do
  pid=$(pgrep -n -x geors || true)
  rss=$( [[ -n $pid ]] && ps -o rss= -p "$pid" | tr -d ' ' ); rss=${rss:-0}
  dir=$(du -sk "$DATA" 2>/dev/null | cut -f1); dir=${dir:-0}
  free=$(df -k "$(dirname "$(realpath "$DATA" 2>/dev/null || echo .)")" | tail -1 | awk '{print $4}')
  swap=$(swap_mb); swap=${swap:-0}
  echo "$(( $(date +%s) - start )) $rss $dir $free $swap" >> "$SAMPLES"
  (( rss > peak_rss )) && peak_rss=$rss
  (( dir > peak_dir )) && peak_dir=$dir
  (( free < min_free )) && min_free=$free
  awk -v s="$swap" -v p="$peak_swap" 'BEGIN {exit !(s > p)}' && peak_swap=$swap
  sleep 15
done
wait $TPID; rc=$?
total=$(( $(date +%s) - start ))

strip() { sed 's/\x1b\[[0-9;]*m//g' "$LOG"; }
{
  echo "geors import: $(basename "$PBF") ($(du -h "$PBF" | cut -f1))"
  echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)   machine: $machine"
  echo "args: $*   exit code: $rc"
  echo
  echo "wall time:             $((total / 60)) min $((total % 60)) s"
  if [[ $(uname) == Darwin ]]; then
    echo "peak memory footprint: $(strip | awk '/peak memory footprint/ {printf "%.2f GB", $1/1073741824}')   (heap)"
    echo "max RSS:               $(strip | awk '/maximum resident/ {printf "%.2f GB", $1/1073741824}')   (includes mmapped scratch / page cache)"
  else
    echo "max RSS:               $(strip | awk -F: '/Maximum resident/ {printf "%.2f GB", $2/1048576}')"
  fi
  echo "peak swap in use:      ${peak_swap} MB"
  echo "peak data dir size:    $(awk -v d="$peak_dir" 'BEGIN {printf "%.1f GB", d/1048576}')   (index + scratch)"
  echo "final data dir size:   $(du -sh "$DATA" 2>/dev/null | cut -f1)"
  echo "lowest free disk:      $(awk -v f="$min_free" 'BEGIN {printf "%.1f GB", f/1048576}')"
  echo
  echo "== phases"
  strip | grep -E "pass [1-5]/5|areas built|features extracted" | sed -E 's/^[0-9T:.-]+Z +INFO [a-z_:]+: //'
  echo
  echo "== countries"
  strip | grep -E "places per country|skipping border" | sed -E 's/^[0-9T:.-]+Z +[A-Z]+ [a-z_:]+: //'
  echo
  echo "== partitions written (slowest 10: seconds, country, places)"
  strip | grep "wrote partition" \
    | sed -E 's/.*partition=([a-z]+) places=([0-9]+).*elapsed=([0-9.]+)(m?s).*/\3\4 \1 \2/' \
    | sort -g -r | head -10
  echo
  echo "== errors / warnings"
  strip | grep -E "ERROR|WARN|Error:" | sed -E 's/^[0-9T:.-]+Z +//' | cut -c1-200 | sort | uniq -c | sort -rn | head -10
  echo
  echo "== partitions"
  "$BIN" info --data "$DATA" 2>/dev/null | grep -E "^[a-z]{2} "
} > "$OUT"
echo "done: $OUT"
