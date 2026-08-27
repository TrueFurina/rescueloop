#!/usr/bin/env bash
set -euo pipefail

task_state_dir=$(mktemp -d)
trap 'rm -rf "$task_state_dir"' EXIT

RUST_LOG=info cargo run --quiet -p rescueloop -- \
  --incident-dir "$task_state_dir/incidents" sources list >/dev/null

if RUST_LOG=info cargo run --quiet -p rescueloop -- \
  --incident-dir "$task_state_dir/incidents" replay "$task_state_dir/missing.json" 2>/dev/null; then
  echo "expected replay failure" >&2
  exit 1
fi

log_file=$(find "$task_state_dir/logs" -name 'rescueloop-*.jsonl' -type f | head -1)
test -n "$log_file"
jq -e 'select(.schema_version == 1 and .run_id and .correlation_id and .fields.event)' \
  "$log_file" >/dev/null
jq -e 'select(.fields.event == "runtime.failed")' "$log_file" >/dev/null

RUST_LOG=info cargo run --quiet -p rescueloop -- \
  --incident-dir "$task_state_dir/incidents" logs --event runtime.failed --output json \
  | jq -e 'select(.fields.event == "runtime.failed")' >/dev/null

echo "Operational logging validation passed."
