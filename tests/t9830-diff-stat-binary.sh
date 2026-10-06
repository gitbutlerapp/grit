#!/bin/sh
# Tests for grit diff --stat output formatting and correctness.

test_description='grit diff --stat output and formatting'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with multiple files' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "text content" >file.txt &&
	echo "second file" >other.txt &&
	echo "third" >third.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial commit"
	)
'

test_expect_success 'setup: modify files' '
	(
	cd repo &&
	echo "text content v2" >file.txt &&
	echo "second file v2" >other.txt &&
	echo "third v2" >third.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "modify files"
	)
'

###########################################################################
# Section 2: Basic --stat output
###########################################################################

test_expect_success 'diff --stat shows changed files' '
	(
	cd repo &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	grep "file.txt" actual &&
	grep "other.txt" actual &&
	grep "third.txt" actual
	)
'

test_expect_success 'diff --stat matches git output' '
	(
	cd repo &&
	"$REAL_GIT" diff --stat HEAD~1 HEAD >expected &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --stat summary shows files changed' '
	(
	cd repo &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	grep "3 files changed" actual
	)
'

test_expect_success 'diff --stat summary shows insertions' '
	(
	cd repo &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	grep "insertion" actual
	)
'

test_expect_success 'diff --stat summary shows deletions' '
	(
	cd repo &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	grep "deletion" actual
	)
'

###########################################################################
# Section 3: Single file changes
###########################################################################

test_expect_success 'setup: single file change' '
	(
	cd repo &&
	echo "text content v3" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "modify one file"
	)
'

test_expect_success 'diff --stat single file matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff --stat HEAD~1 HEAD >expected &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --stat single file shows 1 file changed' '
	(
	cd repo &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	grep "1 file changed" actual
	)
'

###########################################################################
# Section 4: File additions
###########################################################################

test_expect_success 'setup: add new files' '
	(
	cd repo &&
	echo "new a" >new_a.txt &&
	echo "new b" >new_b.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "add new files"
	)
'

test_expect_success 'diff --stat for additions matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff --stat HEAD~1 HEAD >expected &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --stat additions show insertions' '
	(
	cd repo &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	grep "insertion" actual
	)
'

test_expect_success 'diff --stat additions show file names' '
	(
	cd repo &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	grep "new_a.txt" actual &&
	grep "new_b.txt" actual
	)
'

###########################################################################
# Section 5: File deletions
###########################################################################

test_expect_success 'setup: delete files' '
	(
	cd repo &&
	"$REAL_GIT" rm new_a.txt new_b.txt &&
	"$REAL_GIT" commit -m "remove files"
	)
'

test_expect_success 'diff --stat for deletions matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff --stat HEAD~1 HEAD >expected &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --stat deletions show deletions count' '
	(
	cd repo &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	grep "deletion" actual
	)
'

###########################################################################
# Section 6: Multi-line changes
###########################################################################

test_expect_success 'setup: create file with many lines' '
	(
	cd repo &&
	for i in $(seq 1 50); do echo "line $i"; done >bigfile.txt &&
	"$REAL_GIT" add bigfile.txt &&
	"$REAL_GIT" commit -m "add bigfile"
	)
'

test_expect_success 'setup: modify many lines' '
	(
	cd repo &&
	for i in $(seq 1 50); do
		if test $((i % 5)) -eq 0; then
			echo "MODIFIED line $i"
		else
			echo "line $i"
		fi
	done >bigfile.txt &&
	"$REAL_GIT" add bigfile.txt &&
	"$REAL_GIT" commit -m "modify 10 lines in bigfile"
	)
'

test_expect_success 'diff --stat multiline matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff --stat HEAD~1 HEAD >expected &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --stat shows graph bars' '
	(
	cd repo &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	grep "+" actual &&
	grep "-" actual
	)
'

###########################################################################
# Section 7: Renames
###########################################################################

test_expect_success 'setup: rename file' '
	(
	cd repo &&
	"$REAL_GIT" mv file.txt renamed.txt &&
	"$REAL_GIT" commit -m "rename file"
	)
'

test_expect_success 'diff --stat for rename matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff --stat HEAD~1 HEAD >expected &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 8: Subdirectory files
###########################################################################

test_expect_success 'setup: create nested structure' '
	(
	cd repo &&
	mkdir -p a/b/c &&
	echo "deep file" >a/b/c/deep.txt &&
	echo "mid file" >a/b/mid.txt &&
	echo "top file" >a/top.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "add nested files"
	)
'

test_expect_success 'setup: modify nested files' '
	(
	cd repo &&
	echo "deep file v2" >a/b/c/deep.txt &&
	echo "mid file v2" >a/b/mid.txt &&
	echo "top file v2" >a/top.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "modify nested"
	)
'

test_expect_success 'diff --stat nested matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff --stat HEAD~1 HEAD >expected &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --stat shows full paths' '
	(
	cd repo &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	grep "a/b/c/deep.txt" actual &&
	grep "a/b/mid.txt" actual &&
	grep "a/top.txt" actual
	)
'

###########################################################################
# Section 9: Empty commits and identical trees
###########################################################################

test_expect_success 'diff --stat with no changes is empty' '
	(
	cd repo &&
	"$GUST_BIN" diff --stat HEAD HEAD >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 10: Multiple commits range
###########################################################################

test_expect_success 'diff --stat across multiple commits matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff --stat HEAD~5 HEAD >expected &&
	"$GUST_BIN" diff --stat HEAD~5 HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --stat across all history matches git' '
	(
	cd repo &&
	local first=$("$REAL_GIT" rev-list HEAD | tail -1) &&
	"$REAL_GIT" diff --stat "$first" HEAD >expected &&
	"$GUST_BIN" diff --stat "$first" HEAD >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 11: Pathspec with --stat
###########################################################################

test_expect_success 'diff --stat with pathspec matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff --stat HEAD~1 HEAD -- a/ >expected &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD -- a/ >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --stat with single file pathspec matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff --stat HEAD~1 HEAD -- a/top.txt >expected &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD -- a/top.txt >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 12: Insertion-only and deletion-only
###########################################################################

test_expect_success 'setup: insertion-only change' '
	(
	cd repo &&
	echo "extra line" >>bigfile.txt &&
	"$REAL_GIT" add bigfile.txt &&
	"$REAL_GIT" commit -m "append to bigfile"
	)
'

test_expect_success 'diff --stat insertion-only matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff --stat HEAD~1 HEAD >expected &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --stat insertion-only shows no deletions' '
	(
	cd repo &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	! grep "deletion" actual
	)
'

test_expect_success 'setup: deletion-only change' '
	(
	cd repo &&
	head -25 bigfile.txt >tmp && mv tmp bigfile.txt &&
	"$REAL_GIT" add bigfile.txt &&
	"$REAL_GIT" commit -m "truncate bigfile"
	)
'

test_expect_success 'diff --stat deletion-only matches git' '
	(
	cd repo &&
	"$REAL_GIT" diff --stat HEAD~1 HEAD >expected &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --stat deletion-only shows no insertions' '
	(
	cd repo &&
	"$GUST_BIN" diff --stat HEAD~1 HEAD >actual &&
	! grep "insertion" actual
	)
'

test_done
