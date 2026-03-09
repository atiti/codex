#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  ./scripts/update-upstream.sh
  ./scripts/update-upstream.sh --tag rust-v0.112.0
  ./scripts/update-upstream.sh --branch stable-rebase-rust-v0.112.0 --tag rust-v0.112.0

Behavior:
  - With no arguments, rebases main onto upstream/main and pushes origin main.
  - With --tag, rebases the target branch onto upstream/<tag> and does not push.
  - With --branch, rebases that branch instead of the current branch.
EOF
}

branch=""
tag=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --branch)
      branch="${2:-}"
      shift 2
      ;;
    --tag)
      tag="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ -n "$tag" && -z "$branch" ]]; then
  branch="$(git rev-parse --abbrev-ref HEAD)"
fi

if [[ -z "$branch" ]]; then
  branch="main"
fi

if ! git remote | grep -q '^upstream$'; then
  git remote add upstream https://github.com/openai/codex.git
fi

git fetch upstream --tags
git checkout "$branch"

if [[ -n "$tag" ]]; then
  git rebase "upstream/$tag"
else
  git rebase upstream/main
  git push origin "$branch"
fi
