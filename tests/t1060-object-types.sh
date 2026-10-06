#!/bin/sh
# Tests for cat-file -t/-s/-p across blob, tree, commit, and tag object types.

test_description='grit cat-file object type inspection (-t, -s, -p)'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ===========================================================================
# Setup: create a repo with a known blob, tree, commit, and tag
# ===========================================================================

test_expect_success 'setup: init repo and create initial commit' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "Hello World" >hello.txt &&
	mkdir -p sub &&
	echo "sub content" >sub/file.txt &&
	git add hello.txt sub/file.txt &&
	git commit -m "initial commit"
	)
'

test_expect_success 'setup: record object OIDs' '
	(
	cd repo &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	blob_oid=$(git hash-object hello.txt) &&
	echo "$blob_oid" >../blob_oid &&
	tree_oid=$(git rev-parse HEAD^{tree}) &&
	echo "$tree_oid" >../tree_oid &&
	commit_oid=$(git rev-parse HEAD) &&
	echo "$commit_oid" >../commit_oid &&
	git tag -a -m "v1.0 release" v1.0 HEAD &&
	tag_oid=$(git rev-parse v1.0) &&
	echo "$tag_oid" >../tag_oid
	)
'

# ===========================================================================
# Blob: cat-file -t, -s, -p, -e
# ===========================================================================

test_expect_success 'cat-file -t reports blob type' '
	(
	cd repo &&
	echo blob >expect &&
	git cat-file -t "$(cat ../blob_oid)" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s reports blob size' '
	(
	cd repo &&
	echo 12 >expect &&
	git cat-file -s "$(cat ../blob_oid)" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -p prints blob content' '
	(
	cd repo &&
	echo "Hello World" >expect &&
	git cat-file -p "$(cat ../blob_oid)" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -e succeeds for existing blob' '
	(
	cd repo &&
	git cat-file -e "$(cat ../blob_oid)"
	)
'

test_expect_success 'cat-file blob (no flag) prints content' '
	(
	cd repo &&
	echo "Hello World" >expect &&
	git cat-file blob "$(cat ../blob_oid)" >actual &&
	test_cmp expect actual
	)
'

# ===========================================================================
# Tree: cat-file -t, -s, -p
# ===========================================================================

test_expect_success 'cat-file -t reports tree type' '
	(
	cd repo &&
	echo tree >expect &&
	git cat-file -t "$(cat ../tree_oid)" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s reports tree size (nonzero)' '
	(
	cd repo &&
	git cat-file -s "$(cat ../tree_oid)" >actual &&
	size=$(cat actual) &&
	test "$size" -gt 0
	)
'

test_expect_success 'cat-file -p on tree lists entries' '
	(
	cd repo &&
	git cat-file -p "$(cat ../tree_oid)" >actual &&
	grep "hello.txt" actual &&
	grep "sub" actual
	)
'

test_expect_success 'cat-file -p on tree shows blob mode 100644' '
	(
	cd repo &&
	git cat-file -p "$(cat ../tree_oid)" >actual &&
	grep "100644 blob" actual
	)
'

test_expect_success 'cat-file -p on tree shows subtree mode 040000' '
	(
	cd repo &&
	git cat-file -p "$(cat ../tree_oid)" >actual &&
	grep "040000 tree" actual
	)
'

test_expect_success 'cat-file -e succeeds for existing tree' '
	(
	cd repo &&
	git cat-file -e "$(cat ../tree_oid)"
	)
'

# ===========================================================================
# Commit: cat-file -t, -s, -p
# ===========================================================================

test_expect_success 'cat-file -t reports commit type' '
	(
	cd repo &&
	echo commit >expect &&
	git cat-file -t "$(cat ../commit_oid)" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s reports commit size (nonzero)' '
	(
	cd repo &&
	git cat-file -s "$(cat ../commit_oid)" >actual &&
	size=$(cat actual) &&
	test "$size" -gt 0
	)
'

test_expect_success 'cat-file -p on commit shows tree line' '
	(
	cd repo &&
	git cat-file -p "$(cat ../commit_oid)" >actual &&
	grep "^tree $(cat ../tree_oid)" actual
	)
'

test_expect_success 'cat-file -p on commit shows author line' '
	(
	cd repo &&
	git cat-file -p "$(cat ../commit_oid)" >actual &&
	grep "^author Test User <test@example.com>" actual
	)
'

test_expect_success 'cat-file -p on commit shows committer line' '
	(
	cd repo &&
	git cat-file -p "$(cat ../commit_oid)" >actual &&
	grep "^committer Test User <test@example.com>" actual
	)
'

test_expect_success 'cat-file -p on commit shows commit message' '
	(
	cd repo &&
	git cat-file -p "$(cat ../commit_oid)" >actual &&
	grep "initial commit" actual
	)
'

test_expect_success 'cat-file -e succeeds for existing commit' '
	(
	cd repo &&
	git cat-file -e "$(cat ../commit_oid)"
	)
'

test_expect_success 'cat-file commit (type arg) prints commit content' '
	(
	cd repo &&
	git cat-file commit "$(cat ../commit_oid)" >actual &&
	grep "^tree" actual &&
	grep "initial commit" actual
	)
'

# ===========================================================================
# Tag: cat-file -t, -s, -p
# ===========================================================================

test_expect_success 'cat-file -t reports tag type' '
	(
	cd repo &&
	echo tag >expect &&
	git cat-file -t "$(cat ../tag_oid)" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s reports tag size (nonzero)' '
	(
	cd repo &&
	git cat-file -s "$(cat ../tag_oid)" >actual &&
	size=$(cat actual) &&
	test "$size" -gt 0
	)
'

test_expect_success 'cat-file -p on tag shows object line' '
	(
	cd repo &&
	git cat-file -p "$(cat ../tag_oid)" >actual &&
	grep "^object $(cat ../commit_oid)" actual
	)
'

test_expect_success 'cat-file -p on tag shows type commit' '
	(
	cd repo &&
	git cat-file -p "$(cat ../tag_oid)" >actual &&
	grep "^type commit" actual
	)
'

test_expect_success 'cat-file -p on tag shows tag name' '
	(
	cd repo &&
	git cat-file -p "$(cat ../tag_oid)" >actual &&
	grep "^tag v1.0" actual
	)
'

test_expect_success 'cat-file -p on tag shows tagger' '
	(
	cd repo &&
	git cat-file -p "$(cat ../tag_oid)" >actual &&
	grep "^tagger Test User <test@example.com>" actual
	)
'

test_expect_success 'cat-file -p on tag shows tag message' '
	(
	cd repo &&
	git cat-file -p "$(cat ../tag_oid)" >actual &&
	grep "v1.0 release" actual
	)
'

test_expect_success 'cat-file -e succeeds for existing tag' '
	(
	cd repo &&
	git cat-file -e "$(cat ../tag_oid)"
	)
'

test_expect_success 'cat-file tag (type arg) prints tag content' '
	(
	cd repo &&
	git cat-file tag "$(cat ../tag_oid)" >actual &&
	grep "^object" actual &&
	grep "v1.0 release" actual
	)
'

# ===========================================================================
# Negative / edge cases
# ===========================================================================

test_expect_success 'cat-file -e fails for nonexistent object' '
	(
	cd repo &&
	test_must_fail git cat-file -e 0000000000000000000000000000000000000000
	)
'

test_expect_success 'cat-file -t fails for nonexistent object' '
	(
	cd repo &&
	test_must_fail git cat-file -t 0000000000000000000000000000000000000000 2>err &&
	test -s err
	)
'

test_expect_success 'cat-file -p on second commit shows parent line' '
	(
	cd repo &&
	echo "second file" >second.txt &&
	git add second.txt &&
	git commit -m "second commit" &&
	oid2=$(git rev-parse HEAD) &&
	git cat-file -p "$oid2" >actual &&
	grep "^parent $(cat ../commit_oid)" actual
	)
'

test_expect_success 'cat-file -p on empty blob shows empty output' '
	(
	cd repo &&
	empty_oid=$(git hash-object -w /dev/null) &&
	git cat-file -s "$empty_oid" >actual &&
	echo 0 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -t on empty blob still reports blob' '
	(
	cd repo &&
	empty_oid=$(git hash-object -w /dev/null) &&
	echo blob >expect &&
	git cat-file -t "$empty_oid" >actual &&
	test_cmp expect actual
	)
'

test_done
