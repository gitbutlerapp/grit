#!/usr/bin/env bash
# Regenerate grit-lib/tests/fixtures/tree_sha256/golden.json and single-file.bundle.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIX_DIR="$ROOT/grit-lib/tests/fixtures/tree_sha256"
BUNDLE="$FIX_DIR/single-file.bundle"
OUT="$FIX_DIR/golden.json"
mkdir -p "$FIX_DIR"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
git init -q -b main "$TMP/repo"
echo 'golden' >"$TMP/repo/file.txt"
git -C "$TMP/repo" add file.txt
git -C "$TMP/repo" commit -q -m 'golden'
REV="$(git -C "$TMP/repo" rev-parse HEAD)"
git -C "$TMP/repo" bundle create "$BUNDLE" HEAD
DIG="$(python3 - "$TMP/repo" "$REV" <<'PY'
import hashlib, subprocess, sys
fix, rev = sys.argv[1], sys.argv[2]
listing = subprocess.check_output(
    ["git", "-C", fix, "ls-tree", "-r", "-z", "--full-name", rev],
    stderr=subprocess.DEVNULL,
)
records = []
for chunk in listing.split(b"\0"):
    if not chunk:
        continue
    sp = chunk.find(b" ")
    rest = chunk[sp + 1 :]
    sp2 = rest.find(b" ")
    kind = rest[:sp2]
    rest2 = rest[sp2 + 1 :]
    sp3 = rest2.find(b"\t")
    oid_hex = rest2[:sp3].decode()
    path = rest2[sp3 + 1 :]
    if kind in (b"tree", b"commit"):
        continue
    blob = subprocess.check_output(["git", "-C", fix, "cat-file", "-p", oid_hex])
    content_hex = hashlib.sha256(blob).hexdigest()
    records.append(content_hex.encode() + b" " + path + b"\0")
records.sort(key=lambda r: r[r.find(b" ") + 1 : r.rfind(b"\0")])
digest = hashlib.sha256(b"".join(records)).hexdigest()
print(digest)
PY
)"
cat >"$OUT" <<EOF
[
  {
    "name": "single-file",
    "bundle": "tests/fixtures/tree_sha256/single-file.bundle",
    "rev": "$REV",
    "digest": "$DIG"
  }
]
EOF
REF="${TREE_SHA256_BIN:-tree-sha256}"
if command -v "$REF" >/dev/null 2>&1; then
  REF_DIG="$("$REF" --repo "$TMP/repo" HEAD)"
  if [[ "$REF_DIG" != "$DIG" ]]; then
    echo "reference tool digest $REF_DIG != grit $DIG" >&2
    exit 1
  fi
fi
echo "wrote $OUT and $BUNDLE"
