#!/usr/bin/env bash
# Generate seed corpora under grit-lib/fuzz/corpus/ from a temp repo built with system git.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CORPUS="$ROOT/fuzz/corpus"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

write_pkt_line() {
  local out=$1
  local payload=$2
  local len=$((4 + ${#payload} + 1))
  printf '%04x%s\n' "$len" "$payload" >"$out"
}

git init -q "$TMP"
git -C "$TMP" config user.email "fuzz@example.com"
git -C "$TMP" config user.name "Fuzz"

echo "seed blob" >"$TMP/readme.txt"
mkdir -p "$TMP/sub"
echo "nested" >"$TMP/sub/nested.txt"
git -C "$TMP" add -A
git -C "$TMP" commit -q -m "initial"

TREE_OID="$(git -C "$TMP" rev-parse HEAD^{tree})"
COMMIT_OID="$(git -C "$TMP" rev-parse HEAD)"

git -C "$TMP" tag -a v0.1 -m "annotated tag"
TAG_OID="$(git -C "$TMP" rev-parse refs/tags/v0.1)"

mkdir -p "$CORPUS/tree_parse" "$CORPUS/tree_lookup_scan"
git -C "$TMP" cat-file tree "$TREE_OID" >"$CORPUS/tree_parse/git-tree"
cp "$CORPUS/tree_parse/git-tree" "$CORPUS/tree_lookup_scan/git-tree"

mkdir -p "$CORPUS/commit_parse"
git -C "$TMP" cat-file commit "$COMMIT_OID" >"$CORPUS/commit_parse/git-commit"

mkdir -p "$CORPUS/tag_parse"
git -C "$TMP" cat-file tag "$TAG_OID" >"$CORPUS/tag_parse/git-tag"

mkdir -p "$CORPUS/index_parse"
cp "$TMP/.git/index" "$CORPUS/index_parse/git-index"

mkdir -p "$CORPUS/pack_idx_parse"
git -C "$TMP" repack -adq
IDX="$(find "$TMP/.git/objects/pack" -name '*.idx' | head -1)"
cp "$IDX" "$CORPUS/pack_idx_parse/git-pack.idx"

mkdir -p "$CORPUS/packed_refs_parse"
cp "$TMP/.git/packed-refs" "$CORPUS/packed_refs_parse/git-packed-refs" 2>/dev/null || {
  git -C "$TMP" update-ref refs/heads/seed "$COMMIT_OID"
  git -C "$TMP" pack-refs --all --prune
  cp "$TMP/.git/packed-refs" "$CORPUS/packed_refs_parse/git-packed-refs"
}

mkdir -p "$CORPUS/config_parse"
cp "$TMP/.git/config" "$CORPUS/config_parse/git-config"

mkdir -p "$CORPUS/refspec_parse"
printf '%s' '+refs/heads/*:refs/remotes/origin/*' >"$CORPUS/refspec_parse/fetch-wildcard"
printf '%s' 'refs/heads/main:refs/heads/main' >"$CORPUS/refspec_parse/fetch-main"

mkdir -p "$CORPUS/ref_name_check"
printf '%s' 'refs/heads/main' >"$CORPUS/ref_name_check/refs-heads-main"
printf '%s' 'HEAD' >"$CORPUS/ref_name_check/head"
printf '%s' 'refs/tags/v0.1' >"$CORPUS/ref_name_check/refs-tags-v01"
printf '%s' 'feature/foo' >"$CORPUS/ref_name_check/feature-foo"

mkdir -p "$CORPUS/transport_url_parse"
printf '%s' 'https://github.com/git/git.git' >"$CORPUS/transport_url_parse/https-github"
printf '%s' 'file:///tmp/repo.git' >"$CORPUS/transport_url_parse/file-url"
printf '%s' '/var/tmp/local.git' >"$CORPUS/transport_url_parse/local-path"
printf '%s' 'host:path/to/repo' >"$CORPUS/transport_url_parse/scp-style"

mkdir -p "$CORPUS/git_date_parse"
printf '%s' '2.weeks.ago' >"$CORPUS/git_date_parse/two-weeks-ago"
printf '%s' '2024-01-15' >"$CORPUS/git_date_parse/absolute-date"
printf '%s' '@1700000000' >"$CORPUS/git_date_parse/unix-time"
printf '%s' 'yesterday' >"$CORPUS/git_date_parse/yesterday"

mkdir -p "$CORPUS/pkt_line_read" "$CORPUS/protocol_v2_response"
write_pkt_line "$CORPUS/pkt_line_read/v2-cap-line" "version 2"
printf '0000' >>"$CORPUS/pkt_line_read/v2-cap-line"
write_pkt_line "$CORPUS/protocol_v2_response/v2-cap-line" "version 2"
printf '0000' >>"$CORPUS/protocol_v2_response/v2-cap-line"
write_pkt_line "$CORPUS/protocol_v2_response/ls-refs-header" "ls-refs"
printf '0000' >>"$CORPUS/protocol_v2_response/ls-refs-header"

echo "Wrote seed corpus under $CORPUS"
