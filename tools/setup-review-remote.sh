#!/bin/sh
# deckstack local review remote — no network. Creates the bare repo .review.git/
# (gitignored), adds it as remote `review` if absent, and pushes the main branch.
# Usage: sh tools/setup-review-remote.sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

main_branch() {
  if git show-ref --verify --quiet refs/heads/main; then echo main
  elif git show-ref --verify --quiet refs/heads/master; then echo master
  else echo ""; fi
}

MB=$(main_branch)
[ -n "$MB" ] || { echo "setup-review-remote.sh: no main/master branch found" >&2; exit 1; }

if [ ! -d .review.git ]; then
  git init --bare .review.git
  echo "created bare remote .review.git"
else
  echo ".review.git already exists"
fi

if git remote get-url review >/dev/null 2>&1; then
  echo "remote 'review' already set: $(git remote get-url review)"
else
  git remote add review "$ROOT/.review.git"
  echo "added remote 'review' -> $ROOT/.review.git"
fi

git push review "$MB"
echo "pushed $MB to review"
echo "ok: local review remote ready (no network used)"
