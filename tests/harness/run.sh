#!/bin/sh
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(dirname "$(dirname "$SCRIPT_DIR")")"

GUST_BIN="${GUST_BIN:-$REPO_ROOT/target/debug/grit}"
if ! test -x "$GUST_BIN"; then
  echo "Building grit binary at $GUST_BIN"
  cargo build -p grit >/dev/null
fi

echo "Using GUST_BIN=$GUST_BIN"

while IFS= read -r script_name; do
  case "$script_name" in
    ""|\#*) continue ;;
  esac

  echo "==> running tests/$script_name"
  (
    cd "$REPO_ROOT/tests"
    GUST_BIN="$GUST_BIN" TEST_VERBOSE=1 sh "./$script_name"
  )
done <"$SCRIPT_DIR/selected-tests.txt"
