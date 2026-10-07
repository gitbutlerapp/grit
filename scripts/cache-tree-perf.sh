#!/usr/bin/env bash
# Compare system git write-tree vs grit-lib incremental cache-tree on a ~10k-file index.
# Requires: git, release build of grit-lib example (built below), hyperfine optional.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRATCH="${TMPDIR:-/tmp}/grit-cache-tree-perf-$$"
R="$SCRATCH/repo"
FILES="${FILES:-10000}"
DIRS="${DIRS:-10}"

GIT="git -c user.email=t@example.com -c user.name=Test -c commit.gpgsign=false"
GRIT_BIN="$REPO_ROOT/target/release/examples/write_tree_update"

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
)

prepare() {
  rm -f "$R/.git/index"
  $GIT -C "$R" reset -q --hard HEAD
  echo "touch-$(date +%s%N)" >> "$R/d5/f500.txt"
  $GIT -C "$R" add d5/f500.txt
}

echo "Building grit write_tree_update example (release)…"
cargo build -q --release -p grit-lib --example write_tree_update
[[ -x "$GRIT_BIN" ]] || { echo "missing $GRIT_BIN" >&2; exit 1; }

echo "Repo: $FILES files under $R"
echo ""

if command -v hyperfine >/dev/null 2>&1; then
  hyperfine --warmup 2 --min-runs 5 --prepare "$(declare -f prepare); prepare" \
    "$GIT -C $R write-tree" \
    "$GRIT_BIN $R"
else
  prepare
  echo "git write-tree:"
  ( time $GIT -C "$R" write-tree >/dev/null ) 2>&1
  echo ""
  echo "grit write_tree_update:"
  ( time "$GRIT_BIN" "$R" >/dev/null ) 2>&1
fi
