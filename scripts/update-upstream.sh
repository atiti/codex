#!/usr/bin/env bash
set -e

if ! git remote | grep -q '^upstream$'; then
  git remote add upstream https://github.com/openai/codex.git
fi

git fetch upstream
git checkout main
git rebase upstream/main
git push origin main
