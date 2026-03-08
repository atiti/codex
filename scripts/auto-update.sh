#!/usr/bin/env bash

while true
do
    echo "Updating Codex fork..."
    ./scripts/update-upstream.sh
    sleep 86400
done
