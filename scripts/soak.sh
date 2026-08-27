#!/bin/sh
set -eu

duration="${1:-86400}"
sample_interval="${RESCUELOOP_SOAK_INTERVAL:-10}"
max_cpu="${RESCUELOOP_MAX_CPU:-1.0}"
state_dir="$(mktemp -d)"
trap 'if [ -n "${watcher_pid:-}" ]; then kill -TERM "$watcher_pid" 2>/dev/null || true; wait "$watcher_pid" 2>/dev/null || true; fi; rm -rf "$state_dir"' EXIT INT TERM

target/release/rescueloop --incident-dir "$state_dir/incidents" watch >"$state_dir/watch.log" 2>&1 &
watcher_pid=$!
started="$(date +%s)"
samples=0
cpu_sum=0
cpu_max=0

while kill -0 "$watcher_pid" 2>/dev/null; do
  now="$(date +%s)"
  [ $((now - started)) -lt "$duration" ] || break
  cpu="$(ps -p "$watcher_pid" -o %cpu= | awk '{print $1 + 0}')"
  cpu_sum="$(awk -v total="$cpu_sum" -v value="$cpu" 'BEGIN { printf "%.4f", total + value }')"
  cpu_max="$(awk -v peak="$cpu_max" -v value="$cpu" 'BEGIN { print (value > peak ? value : peak) }')"
  samples=$((samples + 1))
  sleep "$sample_interval"
done

kill -0 "$watcher_pid" 2>/dev/null || { echo "watcher exited during soak" >&2; cat "$state_dir/watch.log" >&2; exit 1; }
average="$(awk -v total="$cpu_sum" -v count="$samples" 'BEGIN { if (count == 0) print 0; else printf "%.4f", total / count }')"
kill -TERM "$watcher_pid"
wait "$watcher_pid"
watcher_pid=""
echo "duration=${duration}s samples=$samples avg_cpu=${average}% max_cpu=${cpu_max}%"
awk -v value="$average" -v limit="$max_cpu" 'BEGIN { exit !(value < limit) }' || { echo "average CPU budget exceeded" >&2; exit 1; }
