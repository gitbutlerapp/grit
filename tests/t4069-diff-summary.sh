#!/bin/sh
#
# Tests for grit diff summary output modes:
#   --stat, --numstat, --name-only, --name-status
# Uses diff --cached for index-to-HEAD comparisons and
# diff-tree for commit-to-commit comparisons.

test_description='grit diff summary output formats'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup: create a repo with three commits
# ---------------------------------------------------------------------------
test_expect_success 'setup repository with multiple commits' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&

	echo "line 1" >file1.txt &&
	echo "alpha" >file2.txt &&
	echo "data" >file3.txt &&
	git add . &&
	git commit -m "initial" &&
	git rev-parse HEAD >../c_initial &&

	echo "line 2" >>file1.txt &&
	echo "beta" >file2.txt &&
	git rm -q file3.txt &&
	echo "new-file" >file4.txt &&
	git add . &&
	git commit -m "second" &&
	git rev-parse HEAD >../c_second &&

	echo "line 3" >>file1.txt &&
	echo "line 4" >>file1.txt &&
	echo "line 5" >>file1.txt &&
	echo "gamma" >>file2.txt &&
	echo "extra" >file5.txt &&
	git add . &&
	git commit -m "third" &&
	git rev-parse HEAD >../c_third
	)
'

# ---------------------------------------------------------------------------
# --stat with diff --cached
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached --stat shows modified file' '
	(
	cd repo &&
	echo "more" >>file1.txt &&
	git add file1.txt &&
	grit diff --cached --stat >../out &&
	git reset -q HEAD file1.txt &&
	git checkout -q -- file1.txt &&
	grep "file1.txt" ../out
	)
'

test_expect_success 'diff --cached --stat shows 1 file changed' '
	(
	cd repo &&
	echo "s" >>file2.txt &&
	git add file2.txt &&
	grit diff --cached --stat >../out &&
	git reset -q HEAD file2.txt &&
	git checkout -q -- file2.txt &&
	grep "1 file changed" ../out
	)
'

test_expect_success 'diff --cached --stat for added file' '
	(
	cd repo &&
	echo "brand new" >stat_new.txt &&
	git add stat_new.txt &&
	grit diff --cached --stat >../out &&
	git reset -q HEAD stat_new.txt &&
	rm -f stat_new.txt &&
	grep "stat_new.txt" ../out &&
	grep "1 file changed" ../out
	)
'

test_expect_success 'diff --cached --stat for deleted file' '
	(
	cd repo &&
	git rm -q file4.txt &&
	grit diff --cached --stat >../out &&
	git reset -q HEAD file4.txt &&
	git checkout -q -- file4.txt &&
	grep "file4.txt" ../out &&
	grep "1 file changed" ../out
	)
'

test_expect_success 'diff --cached --stat with multiple files' '
	(
	cd repo &&
	echo "a" >>file1.txt &&
	echo "b" >>file2.txt &&
	git add file1.txt file2.txt &&
	grit diff --cached --stat >../out &&
	git reset -q HEAD file1.txt file2.txt &&
	git checkout -q -- file1.txt file2.txt &&
	grep "2 files changed" ../out
	)
'

test_expect_success 'diff --cached --stat with no changes is empty' '
	(
	cd repo &&
	grit diff --cached --stat >../out &&
	test_must_be_empty ../out
	)
'

# ---------------------------------------------------------------------------
# diff-tree --stat (commit-to-commit)
# ---------------------------------------------------------------------------
test_expect_success 'diff-tree --stat between adjacent commits' '
	(
	cd repo &&
	C2=$(cat ../c_second) &&
	C3=$(cat ../c_third) &&
	grit diff-tree --stat $C2 $C3 >../out &&
	grep "file1.txt" ../out &&
	grep "file5.txt" ../out
	)
'

test_expect_success 'diff-tree --stat between non-adjacent commits' '
	(
	cd repo &&
	C1=$(cat ../c_initial) &&
	C3=$(cat ../c_third) &&
	grit diff-tree --stat $C1 $C3 >../out &&
	grep "file1.txt" ../out &&
	grep "file2.txt" ../out
	)
'

test_expect_success 'diff-tree --stat shows deletion' '
	(
	cd repo &&
	C1=$(cat ../c_initial) &&
	C2=$(cat ../c_second) &&
	grit diff-tree --stat $C1 $C2 >../out &&
	grep "file3.txt" ../out
	)
'

test_expect_success 'diff-tree --stat shows addition' '
	(
	cd repo &&
	C1=$(cat ../c_initial) &&
	C2=$(cat ../c_second) &&
	grit diff-tree --stat $C1 $C2 >../out &&
	grep "file4.txt" ../out
	)
'

# ---------------------------------------------------------------------------
# --numstat with diff --cached
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached --numstat tab-separated format' '
	(
	cd repo &&
	echo "numstat line" >>file1.txt &&
	git add file1.txt &&
	grit diff --cached --numstat >../out &&
	git reset -q HEAD file1.txt &&
	git checkout -q -- file1.txt &&
	grep "1	0	file1.txt" ../out
	)
'

test_expect_success 'diff --cached --numstat for added file' '
	(
	cd repo &&
	echo "hello numstat" >ns_add.txt &&
	git add ns_add.txt &&
	grit diff --cached --numstat >../out &&
	git reset -q HEAD ns_add.txt &&
	rm -f ns_add.txt &&
	grep "1	0	ns_add.txt" ../out
	)
'

test_expect_success 'diff --cached --numstat for deleted file' '
	(
	cd repo &&
	git rm -q file4.txt &&
	grit diff --cached --numstat >../out &&
	git reset -q HEAD file4.txt &&
	git checkout -q -- file4.txt &&
	grep "0	1	file4.txt" ../out
	)
'

test_expect_success 'diff --cached --numstat with multiple files' '
	(
	cd repo &&
	echo "a" >>file1.txt &&
	echo "b" >>file2.txt &&
	git add file1.txt file2.txt &&
	grit diff --cached --numstat >../out &&
	git reset -q HEAD file1.txt file2.txt &&
	git checkout -q -- file1.txt file2.txt &&
	test_line_count = 2 ../out
	)
'

test_expect_success 'diff --cached --numstat with no changes is empty' '
	(
	cd repo &&
	grit diff --cached --numstat >../out &&
	test_must_be_empty ../out
	)
'

# ---------------------------------------------------------------------------
# --name-only with diff --cached
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached --name-only lists modified file' '
	(
	cd repo &&
	echo "changed" >>file2.txt &&
	git add file2.txt &&
	grit diff --cached --name-only >../out &&
	git reset -q HEAD file2.txt &&
	git checkout -q -- file2.txt &&
	echo "file2.txt" >../expect &&
	test_cmp ../expect ../out
	)
'

test_expect_success 'diff --cached --name-only for added file' '
	(
	cd repo &&
	echo "new" >no_new.txt &&
	git add no_new.txt &&
	grit diff --cached --name-only >../out &&
	git reset -q HEAD no_new.txt &&
	rm -f no_new.txt &&
	grep "no_new.txt" ../out
	)
'

test_expect_success 'diff --cached --name-only for deleted file' '
	(
	cd repo &&
	git rm -q file4.txt &&
	grit diff --cached --name-only >../out &&
	git reset -q HEAD file4.txt &&
	git checkout -q -- file4.txt &&
	grep "file4.txt" ../out
	)
'

test_expect_success 'diff --cached --name-only with no changes is empty' '
	(
	cd repo &&
	grit diff --cached --name-only >../out &&
	test_must_be_empty ../out
	)
'

test_expect_success 'diff --cached --name-only with three changed files' '
	(
	cd repo &&
	echo "x" >>file1.txt &&
	echo "y" >>file2.txt &&
	echo "z" >>file5.txt &&
	git add file1.txt file2.txt file5.txt &&
	grit diff --cached --name-only >../out &&
	git reset -q HEAD file1.txt file2.txt file5.txt &&
	git checkout -q -- file1.txt file2.txt file5.txt &&
	test_line_count = 3 ../out
	)
'

# ---------------------------------------------------------------------------
# diff-tree --name-only (commit-to-commit)
# ---------------------------------------------------------------------------
test_expect_success 'diff-tree --name-only between commits' '
	(
	cd repo &&
	C2=$(cat ../c_second) &&
	C3=$(cat ../c_third) &&
	grit diff-tree --name-only $C2 $C3 >../out &&
	grep "file1.txt" ../out &&
	grep "file5.txt" ../out
	)
'

test_expect_success 'diff-tree --name-only includes deleted file' '
	(
	cd repo &&
	C1=$(cat ../c_initial) &&
	C2=$(cat ../c_second) &&
	grit diff-tree --name-only $C1 $C2 >../out &&
	grep "file3.txt" ../out
	)
'

# ---------------------------------------------------------------------------
# --name-status with diff --cached
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached --name-status shows M for modified' '
	(
	cd repo &&
	echo "mod" >>file1.txt &&
	git add file1.txt &&
	grit diff --cached --name-status >../out &&
	git reset -q HEAD file1.txt &&
	git checkout -q -- file1.txt &&
	grep "^M	file1.txt" ../out
	)
'

test_expect_success 'diff --cached --name-status shows A for added' '
	(
	cd repo &&
	echo "add" >ns_added.txt &&
	git add ns_added.txt &&
	grit diff --cached --name-status >../out &&
	git reset -q HEAD ns_added.txt &&
	rm -f ns_added.txt &&
	grep "^A	ns_added.txt" ../out
	)
'

test_expect_success 'diff --cached --name-status shows D for deleted' '
	(
	cd repo &&
	git rm -q file4.txt &&
	grit diff --cached --name-status >../out &&
	git reset -q HEAD file4.txt &&
	git checkout -q -- file4.txt &&
	grep "^D	file4.txt" ../out
	)
'

test_expect_success 'diff --cached --name-status with no changes is empty' '
	(
	cd repo &&
	grit diff --cached --name-status >../out &&
	test_must_be_empty ../out
	)
'

# ---------------------------------------------------------------------------
# diff-tree --name-status (commit-to-commit)
# ---------------------------------------------------------------------------
test_expect_success 'diff-tree --name-status shows A for added file' '
	(
	cd repo &&
	C1=$(cat ../c_initial) &&
	C2=$(cat ../c_second) &&
	grit diff-tree --name-status $C1 $C2 >../out &&
	grep "^A	file4.txt" ../out
	)
'

test_expect_success 'diff-tree --name-status shows D for deleted file' '
	(
	cd repo &&
	C1=$(cat ../c_initial) &&
	C2=$(cat ../c_second) &&
	grit diff-tree --name-status $C1 $C2 >../out &&
	grep "^D	file3.txt" ../out
	)
'

test_expect_success 'diff-tree --name-status shows M for modified file' '
	(
	cd repo &&
	C1=$(cat ../c_initial) &&
	C2=$(cat ../c_second) &&
	grit diff-tree --name-status $C1 $C2 >../out &&
	grep "^M	file1.txt" ../out
	)
'

# ---------------------------------------------------------------------------
# --quiet / --exit-code
# ---------------------------------------------------------------------------
test_expect_success 'diff --quiet with differences exits non-zero' '
	(
	cd repo &&
	echo "noisy" >>file1.txt &&
	git add file1.txt &&
	test_must_fail grit diff --quiet --cached &&
	git reset -q HEAD file1.txt &&
	git checkout -q -- file1.txt
	)
'

test_expect_success 'diff --quiet with no changes exits zero' '
	(
	cd repo &&
	grit diff --quiet --cached
	)
'

test_expect_success 'diff --exit-code with cached changes exits 1' '
	(
	cd repo &&
	echo "exitcode" >>file1.txt &&
	git add file1.txt &&
	test_expect_code 1 grit diff --exit-code --cached &&
	git reset -q HEAD file1.txt &&
	git checkout -q -- file1.txt
	)
'

test_expect_success 'diff --exit-code with no cached changes exits 0' '
	(
	cd repo &&
	grit diff --exit-code --cached
	)
'

test_done
