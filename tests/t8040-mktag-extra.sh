#!/bin/sh
# Tests for mktag edge cases: tagging different object types,
# strict/no-strict mode, various malformed inputs, and encoding.

test_description='mktag edge cases'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository for mktag tests' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo "content" >file.txt &&
	mkdir -p dir &&
	echo "sub" >dir/sub.txt &&
	git add . &&
	git commit -m "initial"
	)
'

test_expect_success 'capture object IDs for tagging' '
	(
	cd repo &&
	git rev-parse HEAD >../commit_id &&
	git rev-parse HEAD^{tree} >../tree_id &&
	git hash-object file.txt >../blob_id
	)
'

# ── Tag a commit object ────────────────────────────────────────────────────

test_expect_success 'mktag creates tag pointing to commit' '
	(
	cd repo &&
	COMMIT=$(cat ../commit_id) &&
	echo "object $COMMIT
type commit
tag v1
tagger T <t@t.com> 1000000000 +0000

tag a commit" | git mktag >../tag_out &&
	test -s ../tag_out
	)
'

test_expect_success 'created tag object is valid' '
	(
	cd repo &&
	TAG=$(cat ../tag_out) &&
	git cat-file -t "$TAG" >../actual &&
	echo "tag" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'tag object content matches input' '
	(
	cd repo &&
	TAG=$(cat ../tag_out) &&
	git cat-file -p "$TAG" >../actual &&
	grep "type commit" ../actual &&
	grep "tag v1" ../actual &&
	grep "tag a commit" ../actual
	)
'

# ── Tag a tree object ──────────────────────────────────────────────────────

test_expect_success 'mktag creates tag pointing to tree' '
	(
	cd repo &&
	TREE=$(cat ../tree_id) &&
	echo "object $TREE
type tree
tag tree-tag
tagger T <t@t.com> 1000000000 +0000

tag a tree" | git mktag >../tree_tag_out &&
	test -s ../tree_tag_out
	)
'

test_expect_success 'tree tag object is valid' '
	(
	cd repo &&
	TAG=$(cat ../tree_tag_out) &&
	git cat-file -t "$TAG" >../actual &&
	echo "tag" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'tree tag references correct type' '
	(
	cd repo &&
	TAG=$(cat ../tree_tag_out) &&
	git cat-file -p "$TAG" >../actual &&
	grep "type tree" ../actual
	)
'

# ── Tag a blob object ──────────────────────────────────────────────────────

test_expect_success 'mktag creates tag pointing to blob' '
	(
	cd repo &&
	BLOB=$(cat ../blob_id) &&
	echo "object $BLOB
type blob
tag blob-tag
tagger T <t@t.com> 1000000000 +0000

tag a blob" | git mktag >../blob_tag_out &&
	test -s ../blob_tag_out
	)
'

test_expect_success 'blob tag object is valid' '
	(
	cd repo &&
	TAG=$(cat ../blob_tag_out) &&
	git cat-file -t "$TAG" >../actual &&
	echo "tag" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'blob tag references correct type' '
	(
	cd repo &&
	TAG=$(cat ../blob_tag_out) &&
	git cat-file -p "$TAG" >../actual &&
	grep "type blob" ../actual
	)
'

# ── Tag a tag object (recursive tagging) ───────────────────────────────────

test_expect_success 'mktag creates tag pointing to another tag' '
	(
	cd repo &&
	TAG=$(cat ../tag_out) &&
	echo "object $TAG
type tag
tag meta-tag
tagger T <t@t.com> 1000000000 +0000

tag of a tag" | git mktag >../meta_tag_out &&
	test -s ../meta_tag_out
	)
'

test_expect_success 'meta-tag references tag type' '
	(
	cd repo &&
	TAG=$(cat ../meta_tag_out) &&
	git cat-file -p "$TAG" >../actual &&
	grep "type tag" ../actual &&
	grep "tag meta-tag" ../actual
	)
'

# ── Strict mode: missing tagger ────────────────────────────────────────────

test_expect_success 'mktag rejects missing tagger in strict mode' '
	(
	cd repo &&
	COMMIT=$(cat ../commit_id) &&
	echo "object $COMMIT
type commit
tag no-tagger

no tagger line" | test_expect_code 1 git mktag
	)
'

test_expect_success 'mktag --no-strict accepts missing tagger' '
	(
	cd repo &&
	COMMIT=$(cat ../commit_id) &&
	echo "object $COMMIT
type commit
tag no-tagger

no tagger line" | git mktag --no-strict >../nostrict_out &&
	test -s ../nostrict_out
	)
'

test_expect_success 'no-strict tag object is still valid' '
	(
	cd repo &&
	TAG=$(cat ../nostrict_out) &&
	git cat-file -t "$TAG" >../actual &&
	echo "tag" >../expect &&
	test_cmp ../expect ../actual
	)
'

# ── Invalid type ────────────────────────────────────────────────────────────

test_expect_success 'mktag rejects invalid type string' '
	(
	cd repo &&
	COMMIT=$(cat ../commit_id) &&
	echo "object $COMMIT
type bogus
tag bad-type
tagger T <t@t.com> 1000000000 +0000

bad type" | test_expect_code 1 git mktag
	)
'

# ── Wrong type for object ──────────────────────────────────────────────────

test_expect_success 'mktag rejects mismatched type (commit tagged as tree)' '
	(
	cd repo &&
	COMMIT=$(cat ../commit_id) &&
	echo "object $COMMIT
type tree
tag wrong-type
tagger T <t@t.com> 1000000000 +0000

wrong type" | test_expect_code 128 git mktag
	)
'

test_expect_success 'mktag rejects mismatched type (tree tagged as commit)' '
	(
	cd repo &&
	TREE=$(cat ../tree_id) &&
	echo "object $TREE
type commit
tag wrong-type2
tagger T <t@t.com> 1000000000 +0000

wrong type" | test_expect_code 128 git mktag
	)
'

test_expect_success 'mktag rejects mismatched type (blob tagged as commit)' '
	(
	cd repo &&
	BLOB=$(cat ../blob_id) &&
	echo "object $BLOB
type commit
tag wrong-type3
tagger T <t@t.com> 1000000000 +0000

wrong type" | test_expect_code 128 git mktag
	)
'

# ── Missing object ─────────────────────────────────────────────────────────

test_expect_success 'mktag rejects nonexistent object' '
	(
	cd repo &&
	echo "object 0000000000000000000000000000000000000000
type commit
tag bad-obj
tagger T <t@t.com> 1000000000 +0000

bad object" | test_expect_code 128 git mktag
	)
'

# ── Missing/empty tag name ──────────────────────────────────────────────────

test_expect_success 'mktag rejects missing tag header' '
	(
	cd repo &&
	COMMIT=$(cat ../commit_id) &&
	echo "object $COMMIT
type commit
tagger T <t@t.com> 1000000000 +0000

no tag name" | test_expect_code 1 git mktag
	)
'

# ── Missing blank line before body ──────────────────────────────────────────

test_expect_success 'mktag rejects tag without blank line before body' '
	(
	cd repo &&
	COMMIT=$(cat ../commit_id) &&
	echo "object $COMMIT
type commit
tag no-blank
tagger T <t@t.com> 1000000000 +0000
no blank line here" | test_expect_code 1 git mktag
	)
'

# ── Empty body ──────────────────────────────────────────────────────────────

test_expect_success 'mktag accepts tag with empty body' '
	(
	cd repo &&
	COMMIT=$(cat ../commit_id) &&
	printf "object %s\ntype commit\ntag empty-body\ntagger T <t@t.com> 1000000000 +0000\n\n" "$COMMIT" | git mktag >../empty_body_out &&
	test -s ../empty_body_out
	)
'

test_expect_success 'empty body tag is valid' '
	(
	cd repo &&
	TAG=$(cat ../empty_body_out) &&
	git cat-file -p "$TAG" >../actual &&
	grep "tag empty-body" ../actual
	)
'

# ── Multi-line tag body ────────────────────────────────────────────────────

test_expect_success 'mktag accepts multi-line body' '
	(
	cd repo &&
	COMMIT=$(cat ../commit_id) &&
	echo "object $COMMIT
type commit
tag multiline
tagger T <t@t.com> 1000000000 +0000

Line one.
Line two.
Line three with special chars: <>&" | git mktag >../multiline_out &&
	test -s ../multiline_out
	)
'

test_expect_success 'multi-line body preserved in tag' '
	(
	cd repo &&
	TAG=$(cat ../multiline_out) &&
	git cat-file -p "$TAG" >../actual &&
	grep "Line one" ../actual &&
	grep "Line two" ../actual &&
	grep "Line three" ../actual
	)
'

# ── Different tagger formats ───────────────────────────────────────────────

test_expect_success 'mktag accepts tagger with positive timezone' '
	(
	cd repo &&
	COMMIT=$(cat ../commit_id) &&
	echo "object $COMMIT
type commit
tag tz-plus
tagger T <t@t.com> 1000000000 +0530

tz test" | git mktag >../tz_out &&
	test -s ../tz_out
	)
'

test_expect_success 'mktag accepts tagger with negative timezone' '
	(
	cd repo &&
	COMMIT=$(cat ../commit_id) &&
	echo "object $COMMIT
type commit
tag tz-minus
tagger T <t@t.com> 1000000000 -0800

tz test" | git mktag >../tz2_out &&
	test -s ../tz2_out
	)
'

test_expect_success 'tagger timezone preserved in tag' '
	(
	cd repo &&
	TAG=$(cat ../tz_out) &&
	git cat-file -p "$TAG" >../actual &&
	grep "+0530" ../actual
	)
'

# ── Completely empty input ──────────────────────────────────────────────────

test_expect_success 'mktag rejects empty input' '
	(
	cd repo &&
	echo "" | test_expect_code 1 git mktag
	)
'

# ── Missing object header ──────────────────────────────────────────────────

test_expect_success 'mktag rejects input without object header' '
	(
	cd repo &&
	echo "type commit
tag no-obj
tagger T <t@t.com> 0 +0000

missing object" | test_expect_code 1 git mktag
	)
'

# ── Tag with update-ref ────────────────────────────────────────────────────

test_expect_success 'tag object can be referenced via update-ref' '
	(
	cd repo &&
	TAG=$(cat ../tag_out) &&
	git update-ref refs/tags/test-tag "$TAG" &&
	git cat-file -t refs/tags/test-tag >../actual &&
	echo "tag" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'tag -v can verify the referenced tag' '
	(
	cd repo &&
	git cat-file -p refs/tags/test-tag >../actual &&
	grep "tag v1" ../actual
	)
'

test_done
