#!/usr/bin/env bash
set -euo pipefail

task_state_dir=$(mktemp -d)
watcher_pid=""
trap 'if [ -n "$watcher_pid" ]; then kill "$watcher_pid" 2>/dev/null || true; fi; rm -rf "$task_state_dir"' EXIT

RUST_LOG=info cargo run --quiet -p rescueloop -- \
  --incident-dir "$task_state_dir/incidents" sources list >/dev/null

if RUST_LOG=info cargo run --quiet -p rescueloop -- \
  --incident-dir "$task_state_dir/incidents" replay "$task_state_dir/missing.json" 2>/dev/null; then
  echo "expected replay failure" >&2
  exit 1
fi

if RESCUELOOP_TEST_PANIC=1 RUST_LOG=info cargo run --quiet -p rescueloop -- \
  --incident-dir "$task_state_dir/incidents" sources list >/dev/null 2>&1; then
  echo "expected debug panic" >&2
  exit 1
fi

RUST_LOG=info target/debug/rescueloop --incident-dir "$task_state_dir/incidents" watch \
  >"$task_state_dir/watcher.out" 2>"$task_state_dir/watcher.err" &
watcher_pid=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do
  grep -Fq 'Status: READY' "$task_state_dir/watcher.out" && break
  sleep 0.2
done
grep -Fq 'Status: READY' "$task_state_dir/watcher.out"
kill -TERM "$watcher_pid"
wait "$watcher_pid"
watcher_pid=""

parallel_pids=""
for _ in 1 2 3 4 5 6 7 8; do
  target/debug/rescueloop --incident-dir "$task_state_dir/incidents" sources list \
    >/dev/null 2>&1 &
  parallel_pids="$parallel_pids $!"
done
for pid in $parallel_pids; do
  wait "$pid"
done

log_file=$(find "$task_state_dir/logs" -name 'rescueloop-*.jsonl' -type f | head -1)
test -n "$log_file"
records_file="$task_state_dir/records.jsonl"
RUST_LOG=info cargo run --quiet -p rescueloop -- \
  --incident-dir "$task_state_dir/incidents" logs --lines 1000 --output json \
  >"$records_file"
jq -e 'select(.schema_version == 1 and .run_id and .correlation_id and .fields.event)' \
  "$records_file" >/dev/null
jq -e 'select(.fields.event == "runtime.failed")' "$records_file" >/dev/null
jq -e 'select(.fields.event == "runtime.panic")' "$records_file" >/dev/null
jq -e 'select(.fields.event == "watch.stopped")' "$records_file" >/dev/null
test "$(jq -r '.run_id' "$records_file" | sort -u | wc -l)" -ge 3

RUST_LOG=info cargo run --quiet -p rescueloop -- \
  --incident-dir "$task_state_dir/incidents" logs --event runtime.failed --output json \
  | jq -e 'select(.fields.event == "runtime.failed")' >/dev/null

RUST_LOG=info cargo run --quiet -p rescueloop -- \
  --incident-dir "$task_state_dir/incidents" logs --verify --lines 0 >/dev/null

echo "Operational logging validation passed."
