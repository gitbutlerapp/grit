#!/bin/sh
#
# Tests for diffing against the empty tree (initial commit scenarios).
# Uses diff-tree --root with the root commit to implicitly diff
# against the empty tree. Always uses -r for recursive traversal.

test_description='grit diff against empty tree (initial commit)'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup repo with root commit' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&

	echo "hello" >file1.txt &&
	echo "world" >file2.txt &&
	mkdir subdir &&
	echo "nested" >subdir/deep.txt &&
	git add . &&
	git commit -m "root commit" &&
	git rev-parse HEAD >../root_oid
	)
'

# ---------------------------------------------------------------------------
# diff-tree --root -r: raw output
# ---------------------------------------------------------------------------
test_expect_success 'diff-tree --root -r shows raw entries for root commit' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	grit diff-tree --root -r $ROOT >../out &&
	grep "file1.txt" ../out &&
	grep "file2.txt" ../out &&
	grep "subdir/deep.txt" ../out
	)
'

test_expect_success 'diff-tree --root -r shows A status for all files' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	grit diff-tree --root -r $ROOT >../out &&
	grep "A	file1.txt" ../out &&
	grep "A	file2.txt" ../out &&
	grep "A	subdir/deep.txt" ../out
	)
'

test_expect_success 'diff-tree --root -r correct file count' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	grit diff-tree --root -r $ROOT >../out &&
	# diff-tree on a commit prints the commit id header line first
	# (matches real git), so 3 files -> 4 lines total.
	test_line_count = 4 ../out
	)
'

test_expect_success 'diff-tree --root -r raw entries start with :000000' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	grit diff-tree --root -r $ROOT >../out &&
	grep "^:000000" ../out
	)
'

# ---------------------------------------------------------------------------
# diff-tree --root -r -p: patch output
# ---------------------------------------------------------------------------
test_expect_success 'diff-tree --root -r -p shows patches' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	grit diff-tree --root -r -p $ROOT >../out &&
	grep "^diff --git" ../out &&
	grep "+hello" ../out &&
	grep "+world" ../out &&
	grep "+nested" ../out
	)
'

test_expect_success 'diff-tree --root -r -p shows new file mode' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	grit diff-tree --root -r -p $ROOT >../out &&
	grep "new file mode" ../out
	)
'

test_expect_success 'diff-tree --root -r -p shows /dev/null as old file' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	grit diff-tree --root -r -p $ROOT >../out &&
	grep "/dev/null" ../out
	)
'

test_expect_success 'diff-tree --root -r -p shows index line' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	grit diff-tree --root -r -p $ROOT >../out &&
	grep "^index 0000000" ../out
	)
'

# ---------------------------------------------------------------------------
# diff-tree --root -r: summary modes
# ---------------------------------------------------------------------------
test_expect_success 'diff-tree --root -r --name-only lists all files' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	grit diff-tree --root -r --name-only $ROOT >../out &&
	grep "file1.txt" ../out &&
	grep "file2.txt" ../out &&
	grep "subdir/deep.txt" ../out
	)
'

test_expect_success 'diff-tree --root -r --name-status shows all A' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	grit diff-tree --root -r --name-status $ROOT >../out &&
	grep "^A	file1.txt" ../out &&
	grep "^A	file2.txt" ../out &&
	grep "^A	subdir/deep.txt" ../out
	)
'

test_expect_success 'diff-tree --root --stat shows stat summary' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	grit diff-tree --root -r --stat $ROOT >../out &&
	grep "file1.txt" ../out &&
	grep "file2.txt" ../out
	)
'

# ---------------------------------------------------------------------------
# Root commit with empty file
# ---------------------------------------------------------------------------
test_expect_success 'setup repo with empty file at root' '
	(
	git init empty-file-repo &&
	cd empty-file-repo &&
	git config user.name "Test" &&
	git config user.email "test@test.com" &&
	>empty.txt &&
	git add empty.txt &&
	git commit -m "add empty file" &&
	git rev-parse HEAD >../empty_root
	)
'

test_expect_success 'diff-tree --root -r with empty file shows entry' '
	(
	cd empty-file-repo &&
	ROOT=$(cat ../empty_root) &&
	grit diff-tree --root -r $ROOT >../out &&
	grep "empty.txt" ../out
	)
'

test_expect_success 'diff-tree --root -r -p with empty file shows diff header' '
	(
	cd empty-file-repo &&
	ROOT=$(cat ../empty_root) &&
	grit diff-tree --root -r -p $ROOT >../out &&
	grep "diff --git" ../out &&
	grep "new file mode" ../out
	)
'

# ---------------------------------------------------------------------------
# Root commit with large file
# ---------------------------------------------------------------------------
test_expect_success 'setup repo with large file at root' '
	(
	git init large-repo &&
	cd large-repo &&
	git config user.name "Test" &&
	git config user.email "test@test.com" &&
	for i in $(seq 1 100); do echo "line $i"; done >bigfile.txt &&
	git add bigfile.txt &&
	git commit -m "add big file" &&
	git rev-parse HEAD >../large_root
	)
'

test_expect_success 'diff-tree --root -r -p with large file shows all content' '
	(
	cd large-repo &&
	ROOT=$(cat ../large_root) &&
	grit diff-tree --root -r -p $ROOT >../out &&
	grep "+line 1" ../out &&
	grep "+line 50" ../out &&
	grep "+line 100" ../out
	)
'

test_expect_success 'diff-tree --root -r --stat with large file' '
	(
	cd large-repo &&
	ROOT=$(cat ../large_root) &&
	grit diff-tree --root -r --stat $ROOT >../out &&
	grep "bigfile.txt" ../out &&
	grep "1 file changed" ../out
	)
'

# ---------------------------------------------------------------------------
# Root commit with many files
# ---------------------------------------------------------------------------
test_expect_success 'setup repo with many files at root' '
	(
	git init many-repo &&
	cd many-repo &&
	git config user.name "Test" &&
	git config user.email "test@test.com" &&
	for i in $(seq 1 10); do echo "content $i" >file_$i.txt; done &&
	git add . &&
	git commit -m "ten files" &&
	git rev-parse HEAD >../many_root
	)
'

test_expect_success 'diff-tree --root -r with many files at root' '
	(
	cd many-repo &&
	ROOT=$(cat ../many_root) &&
	grit diff-tree --root -r $ROOT >../out &&
	# commit id header line + 10 files = 11 lines (matches real git).
	test_line_count = 11 ../out
	)
'

test_expect_success 'diff-tree --root -r --name-only with many files' '
	(
	cd many-repo &&
	ROOT=$(cat ../many_root) &&
	grit diff-tree --root -r --name-only $ROOT >../out &&
	# commit id header line + 10 files = 11 lines (matches real git).
	test_line_count = 11 ../out
	)
'

test_expect_success 'diff-tree --root -r --name-only files sorted' '
	(
	cd many-repo &&
	ROOT=$(cat ../many_root) &&
	grit diff-tree --root -r --name-only $ROOT >../out &&
	sort ../out >../sorted &&
	test_cmp ../sorted ../out
	)
'

# ---------------------------------------------------------------------------
# Comparing root commit with second commit
# ---------------------------------------------------------------------------
test_expect_success 'setup: add second commit to repo' '
	(
	cd repo &&
	echo "modified" >file1.txt &&
	echo "added" >file3.txt &&
	git rm -q file2.txt &&
	git add . &&
	git commit -m "second commit" &&
	git rev-parse HEAD >../second_oid
	)
'

test_expect_success 'diff-tree between root and second: modifications' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	SECOND=$(cat ../second_oid) &&
	grit diff-tree --name-status $ROOT $SECOND >../out &&
	grep "^M	file1.txt" ../out
	)
'

test_expect_success 'diff-tree between root and second: addition' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	SECOND=$(cat ../second_oid) &&
	grit diff-tree --name-status $ROOT $SECOND >../out &&
	grep "^A	file3.txt" ../out
	)
'

test_expect_success 'diff-tree between root and second: deletion' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	SECOND=$(cat ../second_oid) &&
	grit diff-tree --name-status $ROOT $SECOND >../out &&
	grep "^D	file2.txt" ../out
	)
'

test_expect_success 'diff-tree -p between root and second shows patches' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	SECOND=$(cat ../second_oid) &&
	grit diff-tree -p $ROOT $SECOND >../out &&
	grep "^diff --git" ../out
	)
'

test_expect_success 'diff-tree --stat between root and second' '
	(
	cd repo &&
	ROOT=$(cat ../root_oid) &&
	SECOND=$(cat ../second_oid) &&
	grit diff-tree --stat $ROOT $SECOND >../out &&
	grep "file1.txt" ../out
	)
'

# ---------------------------------------------------------------------------
# Nested directories at root
# ---------------------------------------------------------------------------
test_expect_success 'setup repo with deeply nested dirs at root' '
	(
	git init nested-repo &&
	cd nested-repo &&
	git config user.name "Test" &&
	git config user.email "test@test.com" &&
	mkdir -p a/b/c &&
	echo "deep" >a/b/c/file.txt &&
	echo "mid" >a/b/mid.txt &&
	echo "top" >a/top.txt &&
	git add . &&
	git commit -m "nested dirs" &&
	git rev-parse HEAD >../nested_root
	)
'

test_expect_success 'diff-tree --root -r traverses nested dirs' '
	(
	cd nested-repo &&
	ROOT=$(cat ../nested_root) &&
	grit diff-tree --root -r $ROOT >../out &&
	grep "a/b/c/file.txt" ../out &&
	grep "a/b/mid.txt" ../out &&
	grep "a/top.txt" ../out
	)
'

test_expect_success 'diff-tree --root -r --name-only shows full paths' '
	(
	cd nested-repo &&
	ROOT=$(cat ../nested_root) &&
	grit diff-tree --root -r --name-only $ROOT >../out &&
	grep "a/b/c/file.txt" ../out
	)
'

test_expect_success 'diff-tree --root -r -p with nested file shows content' '
	(
	cd nested-repo &&
	ROOT=$(cat ../nested_root) &&
	grit diff-tree --root -r -p $ROOT >../out &&
	grep "+deep" ../out &&
	grep "a/b/c/file.txt" ../out
	)
'

test_done
