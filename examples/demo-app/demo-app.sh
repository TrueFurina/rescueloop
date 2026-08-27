#!/bin/sh
set -eu

if [ -f "examples/demo-app/plugins/broken.plugin" ]; then
  echo "DemoApp: fatal plugin initialization error in examples/demo-app/plugins/broken.plugin" >&2
  exit 42
fi

echo "DemoApp: ready"
