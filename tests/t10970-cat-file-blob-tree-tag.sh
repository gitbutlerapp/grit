#!/bin/sh
# Tests for grit cat-file across blob, tree, commit, and tag objects.

test_description='grit cat-file: blobs, trees, commits, tags, -t, -s, -p flags'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with history and tag' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "hello" >file.txt &&
	mkdir -p sub &&
	echo "nested" >sub/inner.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial" &&
	echo "world" >file2.txt &&
	"$REAL_GIT" add file2.txt &&
	"$REAL_GIT" commit -m "second" &&
	"$REAL_GIT" tag -a v1.0 -m "first release"
	)
'

###########################################################################
# Section 2: cat-file -t (type)
###########################################################################

test_expect_success 'cat-file -t of blob shows blob' '
	(
	cd repo &&
	blob=$(grit rev-parse HEAD:file.txt) &&
	grit cat-file -t "$blob" >actual &&
	echo "blob" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -t of tree shows tree' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	grit cat-file -t "$tree" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -t of commit shows commit' '
	(
	cd repo &&
	grit cat-file -t HEAD >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -t of tag shows tag' '
	(
	cd repo &&
	tag_hash=$("$REAL_GIT" rev-parse v1.0) &&
	grit cat-file -t "$tag_hash" >actual &&
	echo "tag" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 3: cat-file -s (size)
###########################################################################

test_expect_success 'cat-file -s of blob matches git' '
	(
	cd repo &&
	blob=$(grit rev-parse HEAD:file.txt) &&
	grit cat-file -s "$blob" >grit_out &&
	"$REAL_GIT" cat-file -s "$blob" >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'cat-file -s of empty blob is 0' '
	(
	cd repo &&
	hash=$(grit hash-object -w /dev/null) &&
	grit cat-file -s "$hash" >actual &&
	echo "0" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s of tree matches git' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	grit cat-file -s "$tree" >grit_out &&
	"$REAL_GIT" cat-file -s "$tree" >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'cat-file -s of commit matches git' '
	(
	cd repo &&
	commit=$(grit rev-parse HEAD) &&
	grit cat-file -s "$commit" >grit_out &&
	"$REAL_GIT" cat-file -s "$commit" >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'cat-file -s of tag matches git' '
	(
	cd repo &&
	tag_hash=$("$REAL_GIT" rev-parse v1.0) &&
	grit cat-file -s "$tag_hash" >grit_out &&
	"$REAL_GIT" cat-file -s "$tag_hash" >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 4: cat-file -p (pretty-print)
###########################################################################

test_expect_success 'cat-file -p of blob shows content' '
	(
	cd repo &&
	blob=$(grit rev-parse HEAD:file.txt) &&
	grit cat-file -p "$blob" >actual &&
	echo "hello" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -p of blob matches git' '
	(
	cd repo &&
	blob=$(grit rev-parse HEAD:file.txt) &&
	grit cat-file -p "$blob" >grit_out &&
	"$REAL_GIT" cat-file -p "$blob" >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'cat-file -p of tree matches git' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	grit cat-file -p "$tree" >grit_out &&
	"$REAL_GIT" cat-file -p "$tree" >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'cat-file -p of commit contains tree line' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	grit cat-file -p HEAD >actual &&
	grep "^tree $tree" actual
	)
'

test_expect_success 'cat-file -p of commit contains author' '
	(
	cd repo &&
	grit cat-file -p HEAD >actual &&
	grep "^author Test User" actual
	)
'

test_expect_success 'cat-file -p of commit contains committer' '
	(
	cd repo &&
	grit cat-file -p HEAD >actual &&
	grep "^committer Test User" actual
	)
'

test_expect_success 'cat-file -p of commit contains parent' '
	(
	cd repo &&
	parent=$(grit rev-parse HEAD~1) &&
	grit cat-file -p HEAD >actual &&
	grep "^parent $parent" actual
	)
'

test_expect_success 'cat-file -p of commit contains message' '
	(
	cd repo &&
	grit cat-file -p HEAD >actual &&
	grep "second" actual
	)
'

test_expect_success 'cat-file -p of tag contains tag name' '
	(
	cd repo &&
	tag_hash=$("$REAL_GIT" rev-parse v1.0) &&
	grit cat-file -p "$tag_hash" >actual &&
	grep "^tag v1.0" actual
	)
'

test_expect_success 'cat-file -p of tag contains tagger' '
	(
	cd repo &&
	tag_hash=$("$REAL_GIT" rev-parse v1.0) &&
	grit cat-file -p "$tag_hash" >actual &&
	grep "^tagger " actual
	)
'

test_expect_success 'cat-file -p of tag contains message' '
	(
	cd repo &&
	tag_hash=$("$REAL_GIT" rev-parse v1.0) &&
	grit cat-file -p "$tag_hash" >actual &&
	grep "first release" actual
	)
'

###########################################################################
# Section 5: cat-file with type <object> positional form
###########################################################################

test_expect_success 'cat-file blob <hash> shows content' '
	(
	cd repo &&
	blob=$(grit rev-parse HEAD:file.txt) &&
	grit cat-file blob "$blob" >actual &&
	echo "hello" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file commit <hash> shows commit content' '
	(
	cd repo &&
	commit=$(grit rev-parse HEAD) &&
	grit cat-file commit "$commit" >actual &&
	grep "^tree " actual &&
	grep "second" actual
	)
'

###########################################################################
# Section 6: Subtree objects
###########################################################################

test_expect_success 'cat-file -t of subtree shows tree' '
	(
	cd repo &&
	subtree=$(grit rev-parse HEAD:sub) &&
	grit cat-file -t "$subtree" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -p of subtree lists inner.txt' '
	(
	cd repo &&
	subtree=$(grit rev-parse HEAD:sub) &&
	grit cat-file -p "$subtree" >actual &&
	grep "inner.txt" actual
	)
'

test_expect_success 'cat-file -p of nested blob' '
	(
	cd repo &&
	blob=$(grit rev-parse HEAD:sub/inner.txt) &&
	grit cat-file -p "$blob" >actual &&
	echo "nested" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 7: Binary blob roundtrip
###########################################################################

test_expect_success 'cat-file -p of binary blob roundtrips' '
	(
	cd repo &&
	printf "\000\001\377" >binfile &&
	hash=$(grit hash-object -w binfile) &&
	grit cat-file -p "$hash" >actual &&
	cmp binfile actual
	)
'

test_expect_success 'cat-file -s of binary blob is correct' '
	(
	cd repo &&
	printf "\000\001\377" >binfile2 &&
	hash=$(grit hash-object -w binfile2) &&
	grit cat-file -s "$hash" >actual &&
	echo "3" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 8: Error cases
###########################################################################

test_expect_success 'cat-file with invalid hash fails' '
	(
	cd repo &&
	test_must_fail grit cat-file -t 0000000000000000000000000000000000000000 2>err
	)
'

test_expect_success 'cat-file -p with full hash from rev-parse works' '
	(
	cd repo &&
	blob=$(grit rev-parse HEAD:file.txt) &&
	grit cat-file -p "$blob" >actual &&
	echo "hello" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 9: First commit (no parent)
###########################################################################

test_expect_success 'cat-file -p of root commit has no parent line' '
	(
	cd repo &&
	root=$(grit rev-parse HEAD~1) &&
	grit cat-file -p "$root" >actual &&
	! grep "^parent " actual
	)
'

test_expect_success 'cat-file -p of root commit contains initial message' '
	(
	cd repo &&
	root=$(grit rev-parse HEAD~1) &&
	grit cat-file -p "$root" >actual &&
	grep "initial" actual
	)
'

test_done
