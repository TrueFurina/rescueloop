#!/bin/sh
set -eu

duration_seconds="${1:-1800}"
binary="${2:-target/release/rescueloop}"
results="$(mktemp -t rescueloop-perf.XXXXXX)"
state_dir="$(mktemp -d -t rescueloop-state.XXXXXX)"

cleanup() {
  if [ -n "${watcher_pid:-}" ]; then kill "$watcher_pid" 2>/dev/null || true; fi
  rm -f "$results"
  rm -rf "$state_dir"
}
trap cleanup EXIT INT TERM

"$binary" --incident-dir "$state_dir/incidents" watch >/dev/null 2>&1 &
watcher_pid=$!
sleep 1

sample=0
while [ "$sample" -lt "$duration_seconds" ]; do
  ps -o %cpu=,rss= -p "$watcher_pid" >>"$results"
  sample=$((sample + 1))
  sleep 1
done

awk '
  { cpu += $1; if ($1 > max_cpu) max_cpu = $1; rss += $2; if ($2 > max_rss) max_rss = $2; count++ }
  END {
    avg_cpu = cpu / count;
    avg_rss = rss / count / 1024;
    peak_rss = max_rss / 1024;
    printf "samples=%d avg_cpu=%.3f%% max_cpu=%.3f%% avg_rss=%.2fMiB peak_rss=%.2fMiB\n", count, avg_cpu, max_cpu, avg_rss, peak_rss;
    if (avg_cpu >= 1.0 || peak_rss >= 30.0) exit 1;
  }
' "$results"
