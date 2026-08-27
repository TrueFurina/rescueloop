#!/usr/bin/env bash
set -euo pipefail

task_state_dir=$(mktemp -d)
trap 'rm -rf "$task_state_dir"' EXIT

binary_path=$(pwd)/target/debug/rescueloop
config_path="$task_state_dir/inspector.json"
incident_dir="$task_state_dir/.rescueloop/incidents"

cargo build --quiet -p rescueloop
jq -n \
  --arg command "$binary_path" \
  --arg incident_dir "$incident_dir" \
  '{mcpServers:{rescueloop:{type:"stdio",command:$command,args:["--incident-dir",$incident_dir,"mcp"]}}}' \
  >"$config_path"

inspector=(npx -y @modelcontextprotocol/inspector@2.4.0 --cli \
  --config "$config_path" --server rescueloop --format json)

"${inspector[@]}" --method tools/list --strict >/dev/null
"${inspector[@]}" --method tools/call \
  --tool-name list_incidents --tool-arg limit=5 >/dev/null

echo "MCP Inspector validation passed."
