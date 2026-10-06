#!/bin/sh
# Tests for grit diff-tree with --no-commit-id and related options.

test_description='grit diff-tree --no-commit-id output'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repository with history' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "initial" >file1.txt &&
	echo "other" >file2.txt &&
	mkdir -p sub &&
	echo "nested" >sub/file3.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial commit"
	)
'

test_expect_success 'setup: create second commit' '
	(
	cd repo &&
	echo "modified" >file1.txt &&
	echo "new file" >file4.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "second commit"
	)
'

test_expect_success 'setup: create third commit' '
	(
	cd repo &&
	echo "modified again" >file1.txt &&
	"$REAL_GIT" rm file2.txt &&
	echo "sub modified" >sub/file3.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "third commit"
	)
'

###########################################################################
# Section 2: Basic diff-tree
###########################################################################

test_expect_success 'diff-tree HEAD produces output' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree HEAD >actual &&
	test -s actual
	)
'

test_expect_success 'diff-tree basic output matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff-tree HEAD >expected &&
	"$GUST_BIN" diff-tree HEAD >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 3: --no-commit-id
###########################################################################

test_expect_success 'diff-tree --no-commit-id suppresses commit line' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree --no-commit-id HEAD >actual &&
	! head -1 actual | grep "^[0-9a-f]\{40\}$"
	)
'

test_expect_success 'diff-tree --no-commit-id matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff-tree --no-commit-id HEAD >expected &&
	"$GUST_BIN" diff-tree --no-commit-id HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff-tree --no-commit-id -r matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff-tree --no-commit-id -r HEAD >expected &&
	"$GUST_BIN" diff-tree --no-commit-id -r HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff-tree --no-commit-id -r HEAD shows changed files' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree --no-commit-id -r HEAD >actual &&
	grep "file1.txt" actual &&
	grep "file2.txt" actual &&
	grep "file3.txt" actual
	)
'

###########################################################################
# Section 4: --no-commit-id with --name-only
###########################################################################

test_expect_success 'diff-tree --no-commit-id --name-only matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff-tree --no-commit-id --name-only -r HEAD >expected &&
	"$GUST_BIN" diff-tree --no-commit-id --name-only -r HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff-tree --no-commit-id --name-only shows only paths' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree --no-commit-id --name-only -r HEAD >actual &&
	! grep "^:" actual
	)
'

test_expect_success 'diff-tree --no-commit-id --name-only -r HEAD lists 3 files' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree --no-commit-id --name-only -r HEAD >actual &&
	test $(wc -l <actual) -eq 3
	)
'

###########################################################################
# Section 5: --no-commit-id with --name-status
###########################################################################

test_expect_success 'diff-tree --no-commit-id --name-status matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff-tree --no-commit-id --name-status -r HEAD >expected &&
	"$GUST_BIN" diff-tree --no-commit-id --name-status -r HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff-tree --name-status shows M for modified' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree --no-commit-id --name-status -r HEAD >actual &&
	grep "^M" actual | grep "file1.txt"
	)
'

test_expect_success 'diff-tree --name-status shows D for deleted' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree --no-commit-id --name-status -r HEAD >actual &&
	grep "^D" actual | grep "file2.txt"
	)
'

###########################################################################
# Section 6: diff-tree with two tree arguments
###########################################################################

test_expect_success 'diff-tree with two commits matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff-tree HEAD~2 HEAD >expected &&
	"$GUST_BIN" diff-tree HEAD~2 HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff-tree --no-commit-id with two commits matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff-tree --no-commit-id HEAD~2 HEAD >expected &&
	"$GUST_BIN" diff-tree --no-commit-id HEAD~2 HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff-tree -r with two commits matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff-tree -r HEAD~2 HEAD >expected &&
	"$GUST_BIN" diff-tree -r HEAD~2 HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff-tree --no-commit-id -r two commits matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff-tree --no-commit-id -r HEAD~2 HEAD >expected &&
	"$GUST_BIN" diff-tree --no-commit-id -r HEAD~2 HEAD >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 7: diff-tree with tree objects directly
###########################################################################

test_expect_success 'diff-tree with tree hashes matches git' '
	(
	cd repo &&
	local tree1=$("$REAL_GIT" rev-parse HEAD~1^{tree}) &&
	local tree2=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	"$REAL_GIT" diff-tree "$tree1" "$tree2" >expected &&
	"$GUST_BIN" diff-tree "$tree1" "$tree2" >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff-tree --no-commit-id with tree hashes matches git' '
	(
	cd repo &&
	local tree1=$("$REAL_GIT" rev-parse HEAD~1^{tree}) &&
	local tree2=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	"$REAL_GIT" diff-tree --no-commit-id "$tree1" "$tree2" >expected &&
	"$GUST_BIN" diff-tree --no-commit-id "$tree1" "$tree2" >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 8: diff-tree with additions only
###########################################################################

test_expect_success 'setup: commit with only additions' '
	(
	cd repo &&
	echo "brand new A" >added_a.txt &&
	echo "brand new B" >added_b.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "add two files"
	)
'

test_expect_success 'diff-tree --no-commit-id --name-status shows A' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree --no-commit-id --name-status -r HEAD >actual &&
	grep "^A" actual | grep "added_a.txt" &&
	grep "^A" actual | grep "added_b.txt"
	)
'

test_expect_success 'diff-tree additions matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff-tree --no-commit-id --name-status -r HEAD >expected &&
	"$GUST_BIN" diff-tree --no-commit-id --name-status -r HEAD >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 9: diff-tree with deletions only
###########################################################################

test_expect_success 'setup: commit with only deletions' '
	(
	cd repo &&
	"$REAL_GIT" rm added_a.txt added_b.txt &&
	"$REAL_GIT" commit -m "remove two files"
	)
'

test_expect_success 'diff-tree --no-commit-id --name-status shows D' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree --no-commit-id --name-status -r HEAD >actual &&
	grep "^D" actual | grep "added_a.txt" &&
	grep "^D" actual | grep "added_b.txt"
	)
'

test_expect_success 'diff-tree deletions matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff-tree --no-commit-id --name-status -r HEAD >expected &&
	"$GUST_BIN" diff-tree --no-commit-id --name-status -r HEAD >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 10: diff-tree with subdirectory changes
###########################################################################

test_expect_success 'setup: deep directory changes' '
	(
	cd repo &&
	mkdir -p deep/nested/path &&
	echo "deep1" >deep/nested/path/a.txt &&
	echo "deep2" >deep/nested/path/b.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "add deep files"
	)
'

test_expect_success 'diff-tree --no-commit-id -r shows deep paths' '
	(
	cd repo &&
	"$GUST_BIN" diff-tree --no-commit-id --name-only -r HEAD >actual &&
	grep "deep/nested/path/a.txt" actual &&
	grep "deep/nested/path/b.txt" actual
	)
'

test_expect_success 'diff-tree deep paths match git' '
	(
	cd repo &&
	"$REAL_GIT" diff-tree --no-commit-id -r HEAD >expected &&
	"$GUST_BIN" diff-tree --no-commit-id -r HEAD >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 11: diff-tree with pathspec
###########################################################################

test_expect_success 'diff-tree with pathspec matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff-tree --no-commit-id -r HEAD -- deep/ >expected &&
	"$GUST_BIN" diff-tree --no-commit-id -r HEAD -- deep/ >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff-tree with file pathspec matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff-tree --no-commit-id -r HEAD -- deep/nested/path/a.txt >expected &&
	"$GUST_BIN" diff-tree --no-commit-id -r HEAD -- deep/nested/path/a.txt >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 12: Identical trees
###########################################################################

test_expect_success 'diff-tree with identical trees produces empty output' '
	(
	cd repo &&
	local tree=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	"$GUST_BIN" diff-tree "$tree" "$tree" >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff-tree --no-commit-id identical trees empty' '
	(
	cd repo &&
	local tree=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	"$GUST_BIN" diff-tree --no-commit-id "$tree" "$tree" >actual &&
	test_must_be_empty actual
	)
'

test_done
