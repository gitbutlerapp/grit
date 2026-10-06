#!/bin/sh
# Test ls-files output: deduplication, cached/modified/untracked flags,
# staging interactions, and various listing modes.

test_description='grit ls-files deduplication and listing modes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "alpha" >a.txt &&
	echo "beta" >b.txt &&
	echo "gamma" >c.txt &&
	mkdir -p sub &&
	echo "nested" >sub/d.txt &&
	grit add a.txt b.txt c.txt sub/d.txt &&
	grit commit -m "initial"
	)
'

###########################################################################
# Section 2: Basic ls-files (cached)
###########################################################################

test_expect_success 'ls-files lists tracked files' '
	(
	cd repo &&
	grit ls-files >out &&
	grep "a.txt" out &&
	grep "b.txt" out &&
	grep "c.txt" out &&
	grep "sub/d.txt" out
	)
'

test_expect_success 'ls-files shows 4 files' '
	(
	cd repo &&
	grit ls-files >out &&
	test_line_count = 4 out
	)
'

test_expect_success 'ls-files matches git' '
	(
	cd repo &&
	grit ls-files >grit_out &&
	git ls-files >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-files output is sorted' '
	(
	cd repo &&
	grit ls-files >out &&
	sort out >sorted &&
	test_cmp sorted out
	)
'

###########################################################################
# Section 3: No duplicates in basic listing
###########################################################################

test_expect_success 'ls-files has no duplicate entries' '
	(
	cd repo &&
	grit ls-files >out &&
	sort out >sorted &&
	sort -u out >unique &&
	test_cmp sorted unique
	)
'

test_expect_success 'ls-files after re-adding same file has no duplicates' '
	(
	cd repo &&
	grit add a.txt &&
	grit ls-files >out &&
	count=$(grep -c "a.txt" out) &&
	test "$count" = "1"
	)
'

test_expect_success 'ls-files after modifying and re-adding has no duplicates' '
	(
	cd repo &&
	echo "modified alpha" >a.txt &&
	grit add a.txt &&
	grit ls-files >out &&
	count=$(grep -c "a.txt" out) &&
	test "$count" = "1"
	)
'

###########################################################################
# Section 4: ls-files -s (stage info)
###########################################################################

test_expect_success 'ls-files -s shows mode and OID' '
	(
	cd repo &&
	grit ls-files -s >out &&
	grep "100644" out &&
	head -1 out | grep -qE "[0-9a-f]{40}"
	)
'

test_expect_success 'ls-files -s shows stage 0 for normal files' '
	(
	cd repo &&
	grit ls-files -s >out &&
	while read line; do
		stage=$(echo "$line" | awk "{print \$3}") &&
		test "$stage" = "0" || return 1
	done <out
	)
'

test_expect_success 'ls-files -s matches git' '
	(
	cd repo &&
	grit ls-files -s >grit_out &&
	git ls-files -s >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-files -s has no duplicate entries' '
	(
	cd repo &&
	grit ls-files -s >out &&
	sort out >sorted &&
	sort -u out >unique &&
	test_cmp sorted unique
	)
'

###########################################################################
# Section 5: Adding new files
###########################################################################

test_expect_success 'new file appears in ls-files after add' '
	(
	cd repo &&
	echo "new" >new.txt &&
	grit add new.txt &&
	grit ls-files >out &&
	grep "new.txt" out
	)
'

test_expect_success 'file count increases after adding' '
	(
	cd repo &&
	grit ls-files >out &&
	test_line_count = 5 out
	)
'

test_expect_success 'no duplicates after adding new file' '
	(
	cd repo &&
	grit ls-files >out &&
	sort -u out >unique &&
	test_cmp out unique
	)
'

###########################################################################
# Section 6: ls-files with path filter
###########################################################################

test_expect_success 'ls-files with path shows only matching' '
	(
	cd repo &&
	grit ls-files sub/ >out &&
	grep "sub/d.txt" out &&
	! grep "a.txt" out
	)
'

test_expect_success 'ls-files with file path shows single file' '
	(
	cd repo &&
	grit ls-files a.txt >out &&
	test_line_count = 1 out &&
	grep "a.txt" out
	)
'

test_expect_success 'ls-files path filter matches git' '
	(
	cd repo &&
	grit ls-files sub/ >grit_out &&
	git ls-files sub/ >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 7: Untracked files (ls-files -o / --others)
###########################################################################

test_expect_success 'ls-files does not show untracked files' '
	(
	cd repo &&
	echo "untracked1" >untracked1.txt &&
	grit ls-files >out &&
	! grep "untracked1.txt" out
	)
'

test_expect_success 'ls-files only shows indexed files' '
	(
	cd repo &&
	grit ls-files >out &&
	while read f; do
		git ls-files --error-unmatch "$f" 2>/dev/null || return 1
	done <out
	)
'

test_expect_success 'ls-files with --others produces output' '
	(
	cd repo &&
	grit ls-files --others >out &&
	test -s out
	)
'

test_expect_success 'cleanup untracked' '
	(
	cd repo &&
	rm -f untracked1.txt
	)
'

###########################################################################
# Section 8: Deleted files
###########################################################################

test_expect_success 'ls-files still shows deleted file before staging' '
	(
	cd repo &&
	rm c.txt &&
	grit ls-files >out &&
	grep "c.txt" out
	)
'

test_expect_success 'ls-files -d shows deleted files' '
	(
	cd repo &&
	grit ls-files -d >out &&
	grep "c.txt" out
	)
'

test_expect_success 'ls-files -d matches git' '
	(
	cd repo &&
	grit ls-files -d >grit_out &&
	git ls-files -d >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 9: Modified files
###########################################################################

test_expect_success 'ls-files -m shows modified files' '
	(
	cd repo &&
	echo "changed beta" >b.txt &&
	grit ls-files -m >out &&
	grep "b.txt" out
	)
'

test_expect_success 'ls-files -m includes deleted as modified' '
	(
	cd repo &&
	grit ls-files -m >out &&
	grep "c.txt" out
	)
'

test_expect_success 'ls-files -m matches git' '
	(
	cd repo &&
	grit ls-files -m >grit_out &&
	git ls-files -m >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-files -m no duplicates' '
	(
	cd repo &&
	grit ls-files -m >out &&
	sort -u out >unique &&
	test_cmp out unique
	)
'

###########################################################################
# Section 10: Multiple operations, no duplication
###########################################################################

test_expect_success 'restore state for clean tests' '
	(
	cd repo &&
	echo "gamma" >c.txt &&
	echo "beta" >b.txt &&
	grit add b.txt c.txt &&
	rm -f untracked1.txt untracked2.txt
	)
'

test_expect_success 'ls-files after multiple add/modify cycles has no dupes' '
	(
	cd repo &&
	echo "v2" >a.txt && grit add a.txt &&
	echo "v3" >a.txt && grit add a.txt &&
	echo "v4" >a.txt && grit add a.txt &&
	grit ls-files >out &&
	count=$(grep -c "a.txt" out) &&
	test "$count" = "1"
	)
'

test_expect_success 'ls-files -s after multiple add cycles has no dupes' '
	(
	cd repo &&
	grit ls-files -s >out &&
	names=$(awk "{print \$4}" out | sort) &&
	unique_names=$(awk "{print \$4}" out | sort -u) &&
	test "$names" = "$unique_names"
	)
'

test_expect_success 'ls-files after adding many files in subdirs' '
	(
	cd repo &&
	mkdir -p deep/nested/path &&
	for i in $(seq 1 10); do
		echo "content $i" >"deep/nested/path/f$i.txt"
	done &&
	grit add deep/ &&
	grit ls-files deep/ >out &&
	test_line_count = 10 out
	)
'

test_expect_success 'no duplicates in large file set' '
	(
	cd repo &&
	grit ls-files >out &&
	sort out >sorted &&
	sort -u out >unique &&
	test_cmp sorted unique
	)
'

test_expect_success 'total file count is correct' '
	(
	cd repo &&
	grit ls-files >out &&
	grit_count=$(wc -l <out | tr -d " ") &&
	git ls-files >git_out &&
	git_count=$(wc -l <git_out | tr -d " ") &&
	test "$grit_count" = "$git_count"
	)
'

test_expect_success 'full ls-files matches git exactly' '
	(
	cd repo &&
	grit ls-files >grit_out &&
	git ls-files >git_out &&
	test_cmp git_out grit_out
	)
'

test_done
