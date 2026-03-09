#!/usr/bin/env bash
set -euo pipefail

while true; do
  echo "Updating Codex fork..."
  ./scripts/update-upstream.sh "$@"
  sleep 86400
done
