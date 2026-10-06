#!/bin/sh
#
# Tests for 'grit update-index --cacheinfo' — register entries directly by
# mode, object-id, and path without touching the working tree.

test_description='grit update-index --cacheinfo'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup — create a repo with some known objects
# ---------------------------------------------------------------------------
test_expect_success 'setup repository with objects' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "hello world" >file1 &&
	git add file1 &&
	git commit -m "initial" &&
	blob1=$(git rev-parse HEAD:file1) &&
	echo "$blob1" >../blob1_oid &&
	echo "second file" >file2 &&
	git add file2 &&
	git commit -m "second" &&
	blob2=$(git rev-parse HEAD:file2) &&
	echo "$blob2" >../blob2_oid
	)
'

# ---------------------------------------------------------------------------
# Basic --cacheinfo usage
# ---------------------------------------------------------------------------
test_expect_success 'cacheinfo adds entry to empty index' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),newpath" &&
	grit ls-files --stage >actual &&
	grep "newpath" actual
	)
'

test_expect_success 'cacheinfo entry has correct mode' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),modefile" &&
	grit ls-files --stage >actual &&
	grep "^100644" actual
	)
'

test_expect_success 'cacheinfo entry has correct blob oid' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),blobfile" &&
	grit ls-files --stage >actual &&
	grep "$(cat ../blob1_oid)" actual
	)
'

test_expect_success 'cacheinfo can add executable mode 100755' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100755,$(cat ../blob1_oid),script.sh" &&
	grit ls-files --stage >actual &&
	grep "^100755" actual &&
	grep "script.sh" actual
	)
'

test_expect_success 'cacheinfo can add symlink mode 120000' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "120000,$(cat ../blob1_oid),link" &&
	grit ls-files --stage >actual &&
	grep "^120000" actual &&
	grep "link" actual
	)
'

# ---------------------------------------------------------------------------
# Multiple entries
# ---------------------------------------------------------------------------
test_expect_success 'cacheinfo can be used multiple times' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index \
		--cacheinfo "100644,$(cat ../blob1_oid),a.txt" \
		--cacheinfo "100644,$(cat ../blob2_oid),b.txt" &&
	grit ls-files >actual &&
	echo "a.txt" >expect &&
	echo "b.txt" >>expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cacheinfo adds to existing index entries' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),first" &&
	grit update-index --cacheinfo "100644,$(cat ../blob2_oid),second" &&
	grit ls-files >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'cacheinfo replaces entry with same path' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),samefile" &&
	grit update-index --cacheinfo "100644,$(cat ../blob2_oid),samefile" &&
	grit ls-files --stage >actual &&
	grep "$(cat ../blob2_oid)" actual &&
	test_line_count = 1 actual
	)
'

# ---------------------------------------------------------------------------
# Paths with special characters
# ---------------------------------------------------------------------------
test_expect_success 'cacheinfo with subdirectory path' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),dir/sub/file.txt" &&
	grit ls-files >actual &&
	echo "dir/sub/file.txt" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cacheinfo with path containing spaces' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),path with spaces" &&
	grit ls-files >actual &&
	echo "path with spaces" >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# Write-tree after cacheinfo
# ---------------------------------------------------------------------------
test_expect_success 'write-tree succeeds after cacheinfo entries' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),alpha" &&
	grit update-index --cacheinfo "100644,$(cat ../blob2_oid),beta" &&
	tree=$(grit write-tree) &&
	test -n "$tree"
	)
'

test_expect_success 'tree from cacheinfo contains correct entries' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),alpha" &&
	grit update-index --cacheinfo "100644,$(cat ../blob2_oid),beta" &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "alpha" actual &&
	grep "beta" actual
	)
'

# ---------------------------------------------------------------------------
# Error cases
# ---------------------------------------------------------------------------
test_expect_success 'cacheinfo with non-existent oid still updates index' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,0000000000000000000000000000000000000bad,noexist" &&
	grit ls-files --stage >actual &&
	grep "noexist" actual
	)
'

test_expect_success 'cacheinfo fails with no argument' '
	(
	cd repo &&
	test_must_fail grit update-index --cacheinfo 2>err
	)
'

test_expect_success 'cacheinfo fails with malformed argument (missing commas)' '
	(
	cd repo &&
	test_must_fail grit update-index --cacheinfo "100644 $(cat ../blob1_oid) foo"
	)
'

# ---------------------------------------------------------------------------
# Combined with --add
# ---------------------------------------------------------------------------
test_expect_success 'cacheinfo can combine with --add for working tree files' '
	(
	cd repo &&
	rm -f .git/index &&
	echo "wt content" >wtfile &&
	grit update-index --add wtfile &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),ci_only" &&
	grit ls-files >actual &&
	grep "ci_only" actual &&
	grep "wtfile" actual
	)
'

# ---------------------------------------------------------------------------
# info-only (related but different)
# ---------------------------------------------------------------------------
test_expect_success 'update-index --add --info-only adds without file presence' '
	(
	cd repo &&
	rm -f .git/index &&
	echo "ghost" >ghostfile &&
	grit update-index --add --info-only ghostfile &&
	grit ls-files --stage >actual &&
	grep "ghostfile" actual
	)
'

# ---------------------------------------------------------------------------
# index-info from stdin
# ---------------------------------------------------------------------------
test_expect_success 'update-index --index-info reads from stdin' '
	(
	cd repo &&
	rm -f .git/index &&
	echo "100644 $(cat ../blob1_oid)	stdin_file" |
	grit update-index --index-info &&
	grit ls-files >actual &&
	echo "stdin_file" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'index-info multiple entries from stdin' '
	(
	cd repo &&
	rm -f .git/index &&
	printf "100644 %s\t%s\n" "$(cat ../blob1_oid)" "aaa" "$(cat ../blob2_oid)" "bbb" |
	grit update-index --index-info &&
	grit ls-files >actual &&
	test_line_count = 2 actual
	)
'

# ---------------------------------------------------------------------------
# Stage field in cacheinfo
# ---------------------------------------------------------------------------
test_expect_success 'write-tree from cacheinfo is reproducible' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),same" &&
	tree1=$(grit write-tree) &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),same" &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

# ---------------------------------------------------------------------------
# Ordering
# ---------------------------------------------------------------------------
test_expect_success 'ls-files after cacheinfo shows entries sorted' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),zzz" &&
	grit update-index --cacheinfo "100644,$(cat ../blob2_oid),aaa" &&
	grit ls-files >actual &&
	sort actual >sorted &&
	test_cmp sorted actual
	)
'

test_expect_success 'cacheinfo with deeply nested path sorts correctly' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),z/deep/path" &&
	grit update-index --cacheinfo "100644,$(cat ../blob2_oid),a/shallow" &&
	grit ls-files >actual &&
	sort actual >sorted &&
	test_cmp sorted actual
	)
'

# ---------------------------------------------------------------------------
# Mixed mode updates
# ---------------------------------------------------------------------------
test_expect_success 'cacheinfo can change mode of existing entry' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),modchange" &&
	grit ls-files --stage >before &&
	grep "^100644" before &&
	grit update-index --cacheinfo "100755,$(cat ../blob1_oid),modchange" &&
	grit ls-files --stage >after &&
	grep "^100755" after
	)
'

test_expect_success 'cat-file on cacheinfo blob returns correct content' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --cacheinfo "100644,$(cat ../blob1_oid),verify" &&
	grit cat-file -p "$(cat ../blob1_oid)" >actual &&
	echo "hello world" >expect &&
	test_cmp expect actual
	)
'

test_done
