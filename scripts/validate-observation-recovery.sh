#!/bin/sh
set -eu

state_dir="$(mktemp -d)"
trap 'rm -rf "$state_dir"' EXIT
incident_dir="$state_dir/incidents"

cargo build --quiet -p rescueloop
if RESCUELOOP_TEST_ABORT_AFTER_OCCURRENCE=1 target/debug/rescueloop \
  --incident-dir "$incident_dir" run /usr/bin/false >/dev/null 2>&1; then
  echo "expected observation failpoint to abort" >&2
  exit 1
fi

test "$(find "$state_dir/observation-journal" -name '*.json' -type f | wc -l)" -eq 1
test "$(find "$state_dir/occurrences" -name '*.json' -type f | wc -l)" -eq 1
test "$(find "$incident_dir" -name '*.json' -type f 2>/dev/null | wc -l)" -eq 0

target/debug/rescueloop --incident-dir "$incident_dir" run /usr/bin/false >/dev/null

test "$(find "$state_dir/observation-journal" -name '*.json' -type f | wc -l)" -eq 0
test "$(find "$state_dir/occurrences" -name '*.json' -type f | wc -l)" -eq 2
test "$(find "$incident_dir" -name '*.json' -type f | wc -l)" -eq 1
jq -e '.occurrence_count == 2 and .last_occurrence_id' "$incident_dir"/*.json >/dev/null
test "$(wc -l < "$state_dir/repair-ledger.jsonl")" -eq 1

echo "Observation crash recovery validation passed."
