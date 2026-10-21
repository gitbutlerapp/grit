#!/usr/bin/env bash
# Regenerate grit-lib/tests/fixtures/tree_sha256/golden.json using the reference tree-sha256 tool.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/grit-lib/tests/fixtures/tree_sha256/golden.json"
mkdir -p "$(dirname "$OUT")"
REF="${TREE_SHA256_BIN:-tree-sha256}"
if ! command -v "$REF" >/dev/null 2>&1; then
  echo "install reference tool: cargo install --git https://github.com/schacon/tree-sha256 --locked" >&2
  exit 1
fi
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
git init -q -b main "$TMP/repo"
echo 'golden' >"$TMP/repo/file.txt"
git -C "$TMP/repo" add file.txt
git -C "$TMP/repo" commit -q -m 'golden'
DIG="$("$REF" "$TMP/repo")"
REV="$(git -C "$TMP/repo" rev-parse HEAD)"
cat >"$OUT" <<EOF
[
  {
    "name": "single-file",
    "repo": "$TMP/repo",
    "rev": "$REV",
    "digest": "$DIG"
  }
]
EOF
echo "wrote $OUT"
