#!/bin/sh
# Tests for grit diff-tree with various flags and multi-commit comparisons.

test_description='grit diff-tree: raw output, -r, -p, --name-only, --name-status, --stat'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with history' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	mkdir -p sub &&
	echo "c" >sub/c.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "c1" &&
	echo "a2" >a.txt &&
	"$REAL_GIT" add a.txt &&
	"$REAL_GIT" commit -m "c2" &&
	echo "d" >d.txt &&
	"$REAL_GIT" add d.txt &&
	"$REAL_GIT" commit -m "c3" &&
	echo "c2" >sub/c.txt &&
	"$REAL_GIT" add sub/c.txt &&
	"$REAL_GIT" commit -m "c4"
	)
'

###########################################################################
# Section 2: Basic diff-tree raw output
###########################################################################

test_expect_success 'diff-tree -r shows raw diff between two commits' '
	(
	cd repo &&
	grit diff-tree -r HEAD~1 HEAD >actual &&
	grep "sub/c.txt" actual
	)
'

test_expect_success 'diff-tree -r matches git for two-commit form' '
	(
	cd repo &&
	grit diff-tree -r HEAD~1 HEAD >grit_out &&
	"$REAL_GIT" diff-tree -r HEAD~1 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff-tree -r HEAD~2 HEAD shows added and modified' '
	(
	cd repo &&
	grit diff-tree -r HEAD~2 HEAD >actual &&
	grep "d.txt" actual &&
	grep "sub/c.txt" actual
	)
'

test_expect_success 'diff-tree -r HEAD~2 HEAD matches git' '
	(
	cd repo &&
	grit diff-tree -r HEAD~2 HEAD >grit_out &&
	"$REAL_GIT" diff-tree -r HEAD~2 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff-tree -r HEAD~3 HEAD shows all changes' '
	(
	cd repo &&
	grit diff-tree -r HEAD~3 HEAD >actual &&
	grep "a.txt" actual &&
	grep "d.txt" actual &&
	grep "sub/c.txt" actual
	)
'

test_expect_success 'diff-tree -r HEAD~3 HEAD matches git' '
	(
	cd repo &&
	grit diff-tree -r HEAD~3 HEAD >grit_out &&
	"$REAL_GIT" diff-tree -r HEAD~3 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 3: diff-tree --name-only and --name-status
###########################################################################

test_expect_success 'diff-tree --name-only -r shows file names' '
	(
	cd repo &&
	grit diff-tree --name-only -r HEAD~1 HEAD >actual &&
	grep "sub/c.txt" actual
	)
'

test_expect_success 'diff-tree --name-only -r matches git' '
	(
	cd repo &&
	grit diff-tree --name-only -r HEAD~1 HEAD >grit_out &&
	"$REAL_GIT" diff-tree --name-only -r HEAD~1 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff-tree --name-status -r shows M for modification' '
	(
	cd repo &&
	grit diff-tree --name-status -r HEAD~1 HEAD >actual &&
	grep "^M" actual | grep "sub/c.txt"
	)
'

test_expect_success 'diff-tree --name-status -r matches git' '
	(
	cd repo &&
	grit diff-tree --name-status -r HEAD~1 HEAD >grit_out &&
	"$REAL_GIT" diff-tree --name-status -r HEAD~1 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff-tree --name-status shows A for added file' '
	(
	cd repo &&
	grit diff-tree --name-status -r HEAD~2 HEAD >actual &&
	grep "^A" actual | grep "d.txt"
	)
'

test_expect_success 'diff-tree --name-status HEAD~2 HEAD matches git' '
	(
	cd repo &&
	grit diff-tree --name-status -r HEAD~2 HEAD >grit_out &&
	"$REAL_GIT" diff-tree --name-status -r HEAD~2 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff-tree --name-only HEAD~3 HEAD matches git' '
	(
	cd repo &&
	grit diff-tree --name-only -r HEAD~3 HEAD >grit_out &&
	"$REAL_GIT" diff-tree --name-only -r HEAD~3 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff-tree --name-status HEAD~3 HEAD matches git' '
	(
	cd repo &&
	grit diff-tree --name-status -r HEAD~3 HEAD >grit_out &&
	"$REAL_GIT" diff-tree --name-status -r HEAD~3 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 4: diff-tree -p (patch output) for modifications
###########################################################################

test_expect_success 'diff-tree -r -p shows patch for modification' '
	(
	cd repo &&
	grit diff-tree -r -p HEAD~1 HEAD >actual &&
	grep "diff --git" actual &&
	grep "sub/c.txt" actual &&
	grep "^-c$" actual &&
	grep "^+c2$" actual
	)
'

test_expect_success 'diff-tree -r -p modification matches git' '
	(
	cd repo &&
	grit diff-tree -r -p HEAD~1 HEAD >grit_out &&
	"$REAL_GIT" diff-tree -r -p HEAD~1 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff-tree -r -p HEAD~3 HEAD shows multi-file patch' '
	(
	cd repo &&
	grit diff-tree -r -p HEAD~3 HEAD >actual &&
	grep "diff --git" actual &&
	grep "a.txt" actual &&
	grep "sub/c.txt" actual
	)
'

###########################################################################
# Section 5: diff-tree --stat
###########################################################################

test_expect_success 'diff-tree --stat shows summary' '
	(
	cd repo &&
	grit diff-tree --stat -r HEAD~1 HEAD >actual &&
	grep "sub/c.txt" actual &&
	grep "1 file changed" actual
	)
'

test_expect_success 'diff-tree --stat HEAD~3 HEAD shows all files' '
	(
	cd repo &&
	grit diff-tree --stat -r HEAD~3 HEAD >actual &&
	grep "a.txt" actual &&
	grep "d.txt" actual &&
	grep "sub/c.txt" actual &&
	grep "3 files changed" actual
	)
'

###########################################################################
# Section 6: diff-tree with deletion
###########################################################################

test_expect_success 'setup: create commit with deletion' '
	(
	cd repo &&
	"$REAL_GIT" rm b.txt &&
	"$REAL_GIT" commit -m "c5-delete"
	)
'

test_expect_success 'diff-tree -r shows D for deleted file' '
	(
	cd repo &&
	grit diff-tree -r HEAD~1 HEAD >actual &&
	grep "D" actual | grep "b.txt"
	)
'

test_expect_success 'diff-tree -r delete matches git' '
	(
	cd repo &&
	grit diff-tree -r HEAD~1 HEAD >grit_out &&
	"$REAL_GIT" diff-tree -r HEAD~1 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff-tree --name-status delete matches git' '
	(
	cd repo &&
	grit diff-tree --name-status -r HEAD~1 HEAD >grit_out &&
	"$REAL_GIT" diff-tree --name-status -r HEAD~1 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff-tree --name-only delete matches git' '
	(
	cd repo &&
	grit diff-tree --name-only -r HEAD~1 HEAD >grit_out &&
	"$REAL_GIT" diff-tree --name-only -r HEAD~1 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff-tree -p shows deletion patch' '
	(
	cd repo &&
	grit diff-tree -p HEAD~1 HEAD >actual &&
	grep "deleted file" actual &&
	grep "b.txt" actual
	)
'

###########################################################################
# Section 7: diff-tree with new file in subdir
###########################################################################

test_expect_success 'setup: create commit with new file in subdir' '
	(
	cd repo &&
	echo "new nested" >sub/new.txt &&
	"$REAL_GIT" add sub/new.txt &&
	"$REAL_GIT" commit -m "c6-add-nested"
	)
'

test_expect_success 'diff-tree -r shows A for new nested file' '
	(
	cd repo &&
	grit diff-tree -r HEAD~1 HEAD >actual &&
	grep "A" actual | grep "sub/new.txt"
	)
'

test_expect_success 'diff-tree -r new nested file matches git' '
	(
	cd repo &&
	grit diff-tree -r HEAD~1 HEAD >grit_out &&
	"$REAL_GIT" diff-tree -r HEAD~1 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff-tree --name-only new nested file matches git' '
	(
	cd repo &&
	grit diff-tree --name-only -r HEAD~1 HEAD >grit_out &&
	"$REAL_GIT" diff-tree --name-only -r HEAD~1 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff-tree --name-status new nested file matches git' '
	(
	cd repo &&
	grit diff-tree --name-status -r HEAD~1 HEAD >grit_out &&
	"$REAL_GIT" diff-tree --name-status -r HEAD~1 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff-tree -r -p shows new file patch' '
	(
	cd repo &&
	grit diff-tree -r -p HEAD~1 HEAD >actual &&
	grep "new file" actual &&
	grep "sub/new.txt" actual
	)
'

###########################################################################
# Section 8: diff-tree across multiple changes
###########################################################################

test_expect_success 'diff-tree -r across delete+add matches git' '
	(
	cd repo &&
	grit diff-tree -r HEAD~2 HEAD >grit_out &&
	"$REAL_GIT" diff-tree -r HEAD~2 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff-tree --name-status across delete+add matches git' '
	(
	cd repo &&
	grit diff-tree --name-status -r HEAD~2 HEAD >grit_out &&
	"$REAL_GIT" diff-tree --name-status -r HEAD~2 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff-tree --stat across delete+add shows all' '
	(
	cd repo &&
	grit diff-tree --stat -r HEAD~2 HEAD >actual &&
	grep "b.txt" actual &&
	grep "sub/new.txt" actual &&
	grep "2 files changed" actual
	)
'

test_done
