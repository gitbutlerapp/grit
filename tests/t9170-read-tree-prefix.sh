#!/bin/sh
#
# Tests for 'grit read-tree --prefix' — staging a tree into the index under
# a path prefix.

test_description='grit read-tree --prefix (extended)'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup: create repo with files' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo alpha >a.txt &&
	echo beta >b.txt &&
	mkdir sub &&
	echo gamma >sub/c.txt &&
	git add . &&
	git commit -m "initial" &&
	tree=$(git rev-parse HEAD^{tree}) &&
	echo "$tree" >../tree_oid
	)
'

# ---------------------------------------------------------------------------
# Basic prefix read
# ---------------------------------------------------------------------------
test_expect_success 'read-tree --prefix stages under prefix' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=pfx/ "$(cat ../tree_oid)" &&
	grit ls-files >actual &&
	grep "^pfx/a.txt" actual &&
	grep "^pfx/b.txt" actual &&
	grep "^pfx/sub/c.txt" actual
	)
'

test_expect_success 'prefix entries count matches original tree' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree "$(cat ../tree_oid)" &&
	grit ls-files >orig &&
	rm -f .git/index &&
	grit read-tree --prefix=p/ "$(cat ../tree_oid)" &&
	grit ls-files >prefixed &&
	test_line_count = "$(wc -l <orig)" prefixed
	)
'

test_expect_success 'read-tree --prefix twice merges entries' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=one/ "$(cat ../tree_oid)" &&
	grit read-tree --prefix=two/ "$(cat ../tree_oid)" &&
	grit ls-files >actual &&
	grep "^one/a.txt" actual &&
	grep "^two/a.txt" actual
	)
'

test_expect_success 'prefix with nested directory' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=deep/nest/ "$(cat ../tree_oid)" &&
	grit ls-files >actual &&
	grep "^deep/nest/a.txt" actual &&
	grep "^deep/nest/sub/c.txt" actual
	)
'

# ---------------------------------------------------------------------------
# Combining prefix with plain read-tree
# ---------------------------------------------------------------------------
test_expect_success 'read-tree then read-tree --prefix combines' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree "$(cat ../tree_oid)" &&
	grit read-tree --prefix=vendor/ "$(cat ../tree_oid)" &&
	grit ls-files >actual &&
	grep "^a.txt" actual &&
	grep "^vendor/a.txt" actual
	)
'

test_expect_success 'prefix entries do not clobber root entries' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree "$(cat ../tree_oid)" &&
	grit read-tree --prefix=sub2/ "$(cat ../tree_oid)" &&
	grit ls-files >actual &&
	grep "^a.txt" actual &&
	grep "^sub2/a.txt" actual &&
	grep "^sub/c.txt" actual &&
	grep "^sub2/sub/c.txt" actual
	)
'

# ---------------------------------------------------------------------------
# write-tree round-trip
# ---------------------------------------------------------------------------
test_expect_success 'write-tree after prefix read produces valid tree' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=w/ "$(cat ../tree_oid)" &&
	new_tree=$(grit write-tree) &&
	test -n "$new_tree" &&
	grit ls-tree -r "$new_tree" >actual &&
	grep "w/a.txt" actual
	)
'

test_expect_success 'ls-tree of prefixed write-tree has correct paths' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=x/ "$(cat ../tree_oid)" &&
	new_tree=$(grit write-tree) &&
	grit ls-tree -r "$new_tree" >actual &&
	grep "x/b.txt" actual &&
	grep "x/sub/c.txt" actual
	)
'

# ---------------------------------------------------------------------------
# Error cases
# ---------------------------------------------------------------------------
test_expect_success 'read-tree --prefix accepts prefix without trailing slash' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=noslash "$(cat ../tree_oid)" &&
	grit ls-files >actual &&
	grep "^noslash/a.txt" actual
	)
'

test_expect_success 'read-tree --prefix rejects leading slash' '
	(
	cd repo &&
	test_must_fail grit read-tree --prefix=/bad/ "$(cat ../tree_oid)"
	)
'

test_expect_success 'read-tree --prefix=/ fails' '
	(
	cd repo &&
	test_must_fail grit read-tree --prefix=/ "$(cat ../tree_oid)"
	)
'

# ---------------------------------------------------------------------------
# Single-file trees
# ---------------------------------------------------------------------------
test_expect_success 'setup: create single-file tree' '
	(
	cd repo &&
	rm -f .git/index &&
	echo "only" >only.txt &&
	grit update-index --add only.txt &&
	single_tree=$(grit write-tree) &&
	echo "$single_tree" >../single_tree_oid
	)
'

test_expect_success 'prefix with single-file tree works' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=dir/ "$(cat ../single_tree_oid)" &&
	grit ls-files >actual &&
	echo "dir/only.txt" >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# Stage info preserved
# ---------------------------------------------------------------------------
test_expect_success 'prefixed entries have stage 0' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=s/ "$(cat ../tree_oid)" &&
	grit ls-files --stage >actual &&
	! grep "	[123]	" actual
	)
'

test_expect_success 'prefixed entries have correct modes' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=m/ "$(cat ../tree_oid)" &&
	grit ls-files --stage >actual &&
	grep "^100644" actual
	)
'

# ---------------------------------------------------------------------------
# Idempotency
# ---------------------------------------------------------------------------
test_expect_success 'reading same prefix twice fails without duplicating entries' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=dup/ "$(cat ../tree_oid)" &&
	grit ls-files >first &&
	count1=$(wc -l <first) &&
	test_must_fail grit read-tree --prefix=dup/ "$(cat ../tree_oid)" &&
	grit ls-files >second &&
	count2=$(wc -l <second) &&
	test "$count1" = "$count2"
	)
'

# ---------------------------------------------------------------------------
# Large prefix depth
# ---------------------------------------------------------------------------
test_expect_success 'deeply nested prefix a/b/c/d/e/' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=a/b/c/d/e/ "$(cat ../tree_oid)" &&
	grit ls-files >actual &&
	grep "^a/b/c/d/e/a.txt" actual
	)
'

# ---------------------------------------------------------------------------
# Prefix with special characters
# ---------------------------------------------------------------------------
test_expect_success 'prefix with hyphen in name' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=my-lib/ "$(cat ../tree_oid)" &&
	grit ls-files >actual &&
	grep "^my-lib/a.txt" actual
	)
'

test_expect_success 'prefix with underscore' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=my_lib/ "$(cat ../tree_oid)" &&
	grit ls-files >actual &&
	grep "^my_lib/a.txt" actual
	)
'

test_expect_success 'prefix with dots' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=v1.0/ "$(cat ../tree_oid)" &&
	grit ls-files >actual &&
	grep "^v1.0/a.txt" actual
	)
'

# ---------------------------------------------------------------------------
# Tree with only subdirectory content
# ---------------------------------------------------------------------------
test_expect_success 'setup: tree with only nested content' '
	(
	cd repo &&
	rm -f .git/index &&
	mkdir -p deep/dir &&
	echo nested >deep/dir/file &&
	grit update-index --add deep/dir/file &&
	nested_tree=$(grit write-tree) &&
	echo "$nested_tree" >../nested_tree_oid
	)
'

test_expect_success 'prefix on already-nested tree doubles nesting' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=extra/ "$(cat ../nested_tree_oid)" &&
	grit ls-files >actual &&
	grep "^extra/deep/dir/file" actual
	)
'

# ---------------------------------------------------------------------------
# Verify read-tree --prefix + cat-file round-trip
# ---------------------------------------------------------------------------
test_expect_success 'three different prefixes in sequence' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=aa/ "$(cat ../tree_oid)" &&
	grit read-tree --prefix=bb/ "$(cat ../tree_oid)" &&
	grit read-tree --prefix=cc/ "$(cat ../tree_oid)" &&
	grit ls-files >actual &&
	grep "^aa/a.txt" actual &&
	grep "^bb/a.txt" actual &&
	grep "^cc/a.txt" actual
	)
'

test_expect_success 'blob content accessible after prefixed read-tree' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=rt/ "$(cat ../tree_oid)" &&
	grit ls-files --stage >actual &&
	blob=$(grep "rt/a.txt" actual | awk "{print \$2}") &&
	grit cat-file -p "$blob" >content &&
	echo alpha >expect &&
	test_cmp expect content
	)
'

test_done
