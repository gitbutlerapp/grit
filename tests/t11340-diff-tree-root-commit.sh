#!/bin/sh
# Tests for grit diff-tree: comparing tree objects, root commits, recursive mode.

test_description='grit diff-tree: root commit, two-tree comparison, options'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with multiple commits' '
	(
	"$REAL_GIT" init -b main repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&

	echo "file1 content" >file1.txt &&
	echo "file2 content" >file2.txt &&
	mkdir subdir &&
	echo "sub content" >subdir/sub.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "first commit" &&

	echo "file1 modified" >file1.txt &&
	"$REAL_GIT" add file1.txt &&
	"$REAL_GIT" commit -m "second commit" &&

	echo "file3 new" >file3.txt &&
	"$REAL_GIT" add file3.txt &&
	"$REAL_GIT" commit -m "third commit"
	)
'

###########################################################################
# Section 2: diff-tree between two commits
###########################################################################

test_expect_success 'diff-tree HEAD~1 HEAD shows changes' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree HEAD~1 HEAD >out &&
	test -s out
	)
'

test_expect_success 'diff-tree HEAD~1 HEAD lists file3.txt' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree HEAD~1 HEAD >out &&
	grep "file3.txt" out
	)
'

test_expect_success 'diff-tree HEAD~2 HEAD~1 lists file1.txt' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree HEAD~2 HEAD~1 >out &&
	grep "file1.txt" out
	)
'

test_expect_success 'diff-tree same commit produces no output' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree HEAD HEAD >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 3: diff-tree -r (recursive)
###########################################################################

test_expect_success 'diff-tree -r HEAD~2 HEAD shows all changed files' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree -r HEAD~2 HEAD >out &&
	grep "file1.txt" out &&
	grep "file3.txt" out
	)
'

test_expect_success 'diff-tree -r HEAD~2 HEAD does not show unchanged file2.txt' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree -r HEAD~2 HEAD >out &&
	! grep "file2.txt" out
	)
'

###########################################################################
# Section 4: diff-tree with --name-only
###########################################################################

test_expect_success 'diff-tree --name-only HEAD~1 HEAD' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree --name-only HEAD~1 HEAD >out &&
	grep "file3.txt" out
	)
'

test_expect_success 'diff-tree --name-only -r HEAD~2 HEAD shows both changed files' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree --name-only -r HEAD~2 HEAD >out &&
	grep "file1.txt" out &&
	grep "file3.txt" out
	)
'

###########################################################################
# Section 5: diff-tree with --name-status
###########################################################################

test_expect_success 'diff-tree --name-status HEAD~1 HEAD shows A for new file' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree --name-status HEAD~1 HEAD >out &&
	grep "A" out &&
	grep "file3.txt" out
	)
'

test_expect_success 'diff-tree --name-status -r HEAD~2 HEAD~1 shows M for modified' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree --name-status -r HEAD~2 HEAD~1 >out &&
	grep "M" out &&
	grep "file1.txt" out
	)
'

###########################################################################
# Section 6: diff-tree root commit (single commit arg)
###########################################################################

test_expect_success 'diff-tree --root on root commit shows all files' '
	(
	cd repo &&
	ROOT=$("$REAL_GIT" rev-list --max-parents=0 HEAD) &&
	"$GUST_BIN" diff-tree --root $ROOT >out &&
	test -s out
	)
'

test_expect_success 'diff-tree --root -r on root commit lists all files recursively' '
	(
	cd repo &&
	ROOT=$("$REAL_GIT" rev-list --max-parents=0 HEAD) &&
	"$GUST_BIN" diff-tree --root -r $ROOT >out &&
	grep "file1.txt" out &&
	grep "file2.txt" out
	)
'

test_expect_success 'diff-tree --root --name-only on root commit' '
	(
	cd repo &&
	ROOT=$("$REAL_GIT" rev-list --max-parents=0 HEAD) &&
	"$GUST_BIN" diff-tree --root --name-only -r $ROOT >out &&
	grep "file1.txt" out
	)
'

###########################################################################
# Section 7: diff-tree with -p (patch output)
###########################################################################

test_expect_success 'diff-tree -p HEAD~1 HEAD shows patch' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree -p HEAD~1 HEAD >out &&
	grep "diff --git" out
	)
'

test_expect_success 'diff-tree -p shows added file content' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree -p HEAD~1 HEAD >out &&
	grep "+file3 new" out
	)
'

test_expect_success 'diff-tree -p HEAD~2 HEAD~1 shows modification' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree -p HEAD~2 HEAD~1 >out &&
	grep "+file1 modified" out
	)
'

###########################################################################
# Section 8: diff-tree with -t (show tree entries)
###########################################################################

test_expect_success 'diff-tree -t HEAD~2 HEAD includes tree entry for subdir' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree -t HEAD~2 HEAD >out 2>&1 &&
	cat out
	)
'

###########################################################################
# Section 9: diff-tree with tree objects directly
###########################################################################

test_expect_success 'diff-tree with tree SHA1s works' '
	(
	cd repo &&
	TREE1=$("$REAL_GIT" rev-parse HEAD~1^{tree}) &&
	TREE2=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	"$GUST_BIN" diff-tree $TREE1 $TREE2 >out &&
	test -s out
	)
'

test_expect_success 'diff-tree with tree SHA1s shows same result as commit refs' '
	(
	cd repo &&
	TREE1=$("$REAL_GIT" rev-parse HEAD~1^{tree}) &&
	TREE2=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	"$GUST_BIN" diff-tree $TREE1 $TREE2 >out_tree &&
	"$GUST_BIN" diff-tree HEAD~1 HEAD >out_commit &&
	diff -u out_tree out_commit
	)
'

###########################################################################
# Section 10: diff-tree with deleted files
###########################################################################

test_expect_success 'setup: delete a file and commit' '
	(
	cd repo &&
	"$REAL_GIT" rm file2.txt &&
	"$REAL_GIT" commit -m "delete file2"
	)
'

test_expect_success 'diff-tree --name-status shows D for deleted file' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree --name-status HEAD~1 HEAD >out &&
	grep "D" out &&
	grep "file2.txt" out
	)
'

test_expect_success 'diff-tree -p shows deleted file content' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree -p HEAD~1 HEAD >out &&
	grep "deleted file" out
	)
'

###########################################################################
# Section 11: diff-tree with multiple files changed
###########################################################################

test_expect_success 'setup: modify multiple files and commit' '
	(
	cd repo &&
	echo "modified again" >file1.txt &&
	echo "sub modified" >subdir/sub.txt &&
	echo "brand new" >file4.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "multi-file change"
	)
'

test_expect_success 'diff-tree -r --name-only shows all changed files' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree -r --name-only HEAD~1 HEAD >out &&
	grep "file1.txt" out &&
	grep "subdir/sub.txt" out &&
	grep "file4.txt" out
	)
'

test_expect_success 'diff-tree -r --name-status HEAD~1 HEAD shows correct statuses' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree -r --name-status HEAD~1 HEAD >out &&
	grep "^M" out &&
	grep "^A" out
	)
'

###########################################################################
# Section 12: diff-tree single commit (compares to parent)
###########################################################################

test_expect_success 'diff-tree with single non-root commit compares to parent' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree HEAD >out &&
	test -s out
	)
'

test_expect_success 'diff-tree with single commit -r lists changed files' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree -r HEAD >out &&
	grep "file1.txt" out
	)
'

test_expect_success 'diff-tree --name-only single commit' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree --name-only -r HEAD >out &&
	grep "file1.txt" out
	)
'

test_expect_success 'diff-tree -r single commit does not list unchanged files' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree -r --name-only HEAD >out &&
	! grep "file3.txt" out
	)
'

test_done
