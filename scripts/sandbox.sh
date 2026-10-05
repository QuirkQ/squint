#!/usr/bin/env bash
# Opens and closes test pull requests in a private sandbox repository, to try
# Squint's review actions on something harmless.
#
#   sandbox.sh pr [--draft]   a PR by github-actions[bot], with your review requested
#   sandbox.sh own            a PR by you: you can comment, but GitHub refuses your approval
#   sandbox.sh reset          close every open PR and delete its branch
#
# The repository is $SQUINT_SANDBOX, else <your login>/squint-sandbox. Its
# README describes the change each PR makes.
set -euo pipefail

repo=${SQUINT_SANDBOX:-$(gh api user --jq .login)/squint-sandbox}
id=$(date +%Y%m%d-%H%M%S)

# Runs the sandbox's workflow, which pushes test/<id>, then polls `check`
# until it succeeds.
dispatch() {
  local check=$1
  shift
  gh workflow run open-test-pr.yml -R "$repo" -f id="$id" "$@" >/dev/null
  echo "Waiting for the sandbox's workflow…" >&2
  for _ in $(seq 60); do
    $check 2>/dev/null && return
    sleep 3
  done
  echo "Nothing after 3 minutes, see https://github.com/$repo/actions" >&2
  exit 1
}

pr_url() { gh pr view "test/$id" -R "$repo" --json url --jq .url; }
branch_pushed() { gh api "repos/$repo/branches/test/$id" --silent; }

case ${1:-} in
pr)
  draft=false
  [[ ${2:-} == --draft ]] && draft=true
  dispatch pr_url -f draft="$draft"
  ;;
own)
  dispatch branch_pushed -f open=false
  gh pr create -R "$repo" --head "test/$id" --title "Own sandbox PR $id" \
    --body "You opened this one, so GitHub lets you comment on it but not approve it or request changes."
  ;;
reset)
  gh pr list -R "$repo" --state open --limit 100 --json number --jq '.[].number' |
    while read -r number; do
      gh pr close "$number" -R "$repo" --delete-branch
    done
  ;;
*)
  sed -n '4,7p' "$0" | sed 's/^#   //' >&2
  exit 1
  ;;
esac
