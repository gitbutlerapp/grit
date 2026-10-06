#!/bin/sh
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(dirname "$(dirname "$SCRIPT_DIR")")"

python3 "$SCRIPT_DIR/refresh_from_upstream.py"

echo "Refreshed selected tests from git/t into tests/."
echo "Included scripts:"
sed 's/^/- /' "$SCRIPT_DIR/selected-tests.txt"
echo
echo "To refresh from upstream again:"
echo "  tests/harness/refresh.sh"
