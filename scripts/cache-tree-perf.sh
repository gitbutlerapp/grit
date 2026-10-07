#!/usr/bin/env bash
# Compare git write-tree vs grit-lib incremental cache-tree on a large index.
# Requires: git, hyperfine (optional), release build of grit-lib tests via cargo.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRATCH="${TMPDIR:-/tmp}/grit-cache-tree-perf-$$"
R="$SCRATCH/repo"
FILES="${FILES:-10000}"
DIRS="${DIRS:-10}"

GIT="git -c user.email=t@example.com -c user.name=Test -c commit.gpgsign=false"

cleanup() { rm -rf "$SCRATCH"; }
trap cleanup EXIT

mkdir -p "$R"
(
  cd "$R"
  $GIT init -q -b main
  per=$(( (FILES + DIRS - 1) / DIRS ))
  made=0
  for d in $(seq 0 $((DIRS - 1))); do
    mkdir -p "d$d"
    for f in $(seq 0 $((per - 1))); do
      made=$((made + 1))
      [[ $made -gt $FILES ]] && break
      printf 'c %s/%s\n' "$d" "$f" > "d$d/f$f.txt"
    done
  done
  $GIT add -A
  $GIT commit -qm "initial $FILES"
  echo "touch" >> d5/f500.txt
  $GIT add d5/f500.txt
)

prepare() {
  rm -f "$R/.git/index"
  $GIT -C "$R" reset -q --hard HEAD
  echo "touch-$(date +%s%N)" >> "$R/d5/f500.txt"
  $GIT -C "$R" add d5/f500.txt
}

echo "Repo: $FILES files under $R"

if command -v hyperfine >/dev/null 2>&1; then
  hyperfine --warmup 2 --min-runs 5 --prepare "$(declare -f prepare); prepare" \
    "$GIT -C $R write-tree" \
    "cargo test -q -p grit-lib --test cache_tree_git_compat grit_cache_tree_matches_git_write_tree_after_single_path_change -- --exact --nocapture"
else
  prepare
  echo "git write-tree:"
  ( time $GIT -C "$R" write-tree >/dev/null ) 2>&1
fi
