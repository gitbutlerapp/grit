#!/bin/sh
# Tests for grit diff --cached with renames, new files, deleted files, and mode changes.
# grit now detects renames, matching git behavior.

test_description='grit diff --cached: renames, new files, deleted files, mixed changes'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with multiple files' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "line1" >a.txt &&
	echo "line2" >>a.txt &&
	echo "line3" >>a.txt &&
	echo "content of b" >b.txt &&
	echo "content of c" >c.txt &&
	mkdir -p dir &&
	echo "nested" >dir/d.txt &&
	echo "also nested" >dir/e.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial"
	)
'

###########################################################################
# Section 2: Simple staged rename via git mv
###########################################################################

test_expect_success 'diff --cached --name-status after git mv shows rename' '
	(
	cd repo &&
	"$REAL_GIT" mv a.txt renamed.txt &&
	grit diff --cached --name-status >actual &&
	grep "^R" actual | grep "a.txt" &&
	grep "^R" actual | grep "renamed.txt"
	)
'

test_expect_success 'diff --cached --name-only shows renamed file' '
	(
	cd repo &&
	grit diff --cached --name-only >actual &&
	grep "renamed.txt" actual
	)
'

test_expect_success 'diff --cached shows rename header for rename' '
	(
	cd repo &&
	grit diff --cached >actual &&
	grep "rename from a.txt" actual &&
	grep "rename to renamed.txt" actual
	)
'

test_expect_success 'diff --cached --numstat shows stats for rename' '
	(
	cd repo &&
	grit diff --cached --numstat >actual &&
	grep "renamed.txt\|a.txt" actual
	)
'

test_expect_success 'diff --cached --stat shows summary for rename' '
	(
	cd repo &&
	grit diff --cached --stat >actual &&
	grep "renamed.txt\|a.txt" actual &&
	grep "1 file changed" actual
	)
'

test_expect_success 'diff --cached --exit-code returns 1 for rename' '
	(
	cd repo &&
	test_must_fail grit diff --cached --exit-code
	)
'

test_expect_success 'setup: reset rename' '
	(
	cd repo &&
	"$REAL_GIT" reset HEAD -- . >/dev/null 2>&1 &&
	"$REAL_GIT" checkout -- . &&
	rm -f renamed.txt
	)
'

###########################################################################
# Section 3: Staged modification to multi-line file
###########################################################################

test_expect_success 'diff --cached shows hunk for modified multi-line file' '
	(
	cd repo &&
	echo "LINE1" >a.txt &&
	echo "line2" >>a.txt &&
	echo "line3" >>a.txt &&
	"$REAL_GIT" add a.txt &&
	grit diff --cached >actual &&
	grep "^-line1" actual &&
	grep "^+LINE1" actual
	)
'

test_expect_success 'diff --cached multi-line modification matches git' '
	(
	cd repo &&
	grit diff --cached >grit_out &&
	"$REAL_GIT" diff --cached >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached -U1 matches git for multi-line' '
	(
	cd repo &&
	grit diff --cached -U1 >grit_out &&
	"$REAL_GIT" diff --cached -U1 >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached -U0 matches git for multi-line' '
	(
	cd repo &&
	grit diff --cached -U0 >grit_out &&
	"$REAL_GIT" diff --cached -U0 >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached --name-status shows M for modification' '
	(
	cd repo &&
	grit diff --cached --name-status >grit_out &&
	"$REAL_GIT" diff --cached --name-status >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached --numstat matches git for modification' '
	(
	cd repo &&
	grit diff --cached --numstat >grit_out &&
	"$REAL_GIT" diff --cached --numstat >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'setup: reset modification' '
	(
	cd repo &&
	"$REAL_GIT" reset HEAD -- . >/dev/null 2>&1 &&
	"$REAL_GIT" checkout -- .
	)
'

###########################################################################
# Section 4: Staged new file
###########################################################################

test_expect_success 'diff --cached shows new file' '
	(
	cd repo &&
	echo "brand new" >new.txt &&
	"$REAL_GIT" add new.txt &&
	grit diff --cached >actual &&
	grep "new file" actual &&
	grep "new.txt" actual
	)
'

test_expect_success 'diff --cached --name-status shows A for new file' '
	(
	cd repo &&
	grit diff --cached --name-status >actual &&
	grep "^A" actual | grep "new.txt"
	)
'

test_expect_success 'diff --cached --name-status new file matches git' '
	(
	cd repo &&
	grit diff --cached --name-status >grit_out &&
	"$REAL_GIT" diff --cached --name-status >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached --numstat new file matches git' '
	(
	cd repo &&
	grit diff --cached --numstat >grit_out &&
	"$REAL_GIT" diff --cached --numstat >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached --name-only new file matches git' '
	(
	cd repo &&
	grit diff --cached --name-only >grit_out &&
	"$REAL_GIT" diff --cached --name-only >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'setup: reset new file' '
	(
	cd repo &&
	"$REAL_GIT" reset HEAD -- new.txt >/dev/null 2>&1 &&
	rm -f new.txt
	)
'

###########################################################################
# Section 5: Staged deletion
###########################################################################

test_expect_success 'diff --cached shows deleted file' '
	(
	cd repo &&
	"$REAL_GIT" rm c.txt &&
	grit diff --cached >actual &&
	grep "deleted file" actual &&
	grep "c.txt" actual
	)
'

test_expect_success 'diff --cached --name-status shows D for deleted file' '
	(
	cd repo &&
	grit diff --cached --name-status >actual &&
	grep "^D" actual | grep "c.txt"
	)
'

test_expect_success 'diff --cached deletion matches git --name-status' '
	(
	cd repo &&
	grit diff --cached --name-status >grit_out &&
	"$REAL_GIT" diff --cached --name-status >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached deletion matches git --numstat' '
	(
	cd repo &&
	grit diff --cached --numstat >grit_out &&
	"$REAL_GIT" diff --cached --numstat >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached deletion matches git --name-only' '
	(
	cd repo &&
	grit diff --cached --name-only >grit_out &&
	"$REAL_GIT" diff --cached --name-only >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'setup: reset deletion' '
	(
	cd repo &&
	"$REAL_GIT" reset HEAD -- . >/dev/null 2>&1 &&
	"$REAL_GIT" checkout -- .
	)
'

###########################################################################
# Section 6: Multiple files modified in different subdirs
###########################################################################

test_expect_success 'diff --cached with multiple subdir modifications' '
	(
	cd repo &&
	echo "modified b" >b.txt &&
	echo "modified d" >dir/d.txt &&
	echo "modified e" >dir/e.txt &&
	"$REAL_GIT" add b.txt dir/d.txt dir/e.txt &&
	grit diff --cached --name-only >actual &&
	grep "b.txt" actual &&
	grep "dir/d.txt" actual &&
	grep "dir/e.txt" actual
	)
'

test_expect_success 'diff --cached --name-only multiple matches git' '
	(
	cd repo &&
	grit diff --cached --name-only >grit_out &&
	"$REAL_GIT" diff --cached --name-only >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached --name-status multiple matches git' '
	(
	cd repo &&
	grit diff --cached --name-status >grit_out &&
	"$REAL_GIT" diff --cached --name-status >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached --numstat multiple matches git' '
	(
	cd repo &&
	grit diff --cached --numstat >grit_out &&
	"$REAL_GIT" diff --cached --numstat >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached multiple matches git patch' '
	(
	cd repo &&
	grit diff --cached >grit_out &&
	"$REAL_GIT" diff --cached >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached --stat multiple shows all files' '
	(
	cd repo &&
	grit diff --cached --stat >actual &&
	grep "b.txt" actual &&
	grep "dir/d.txt" actual &&
	grep "dir/e.txt" actual &&
	grep "3 files changed" actual
	)
'

test_expect_success 'diff --cached --exit-code returns 1 with changes' '
	(
	cd repo &&
	test_must_fail grit diff --cached --exit-code
	)
'

test_expect_success 'diff --cached --quiet returns 1 with changes' '
	(
	cd repo &&
	test_must_fail grit diff --cached --quiet >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'setup: commit multiple changes' '
	(
	cd repo &&
	"$REAL_GIT" commit -m "multiple changes"
	)
'

test_expect_success 'diff --cached shows nothing after commit' '
	(
	cd repo &&
	grit diff --cached >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff --cached --exit-code returns 0 when clean' '
	(
	cd repo &&
	grit diff --cached --exit-code
	)
'

###########################################################################
# Section 7: Empty file operations
###########################################################################

test_expect_success 'diff --cached handles empty new file' '
	(
	cd repo &&
	>empty.txt &&
	"$REAL_GIT" add empty.txt &&
	grit diff --cached --name-status >actual &&
	grep "^A" actual | grep "empty.txt"
	)
'

test_expect_success 'diff --cached empty file matches git --name-status' '
	(
	cd repo &&
	grit diff --cached --name-status >grit_out &&
	"$REAL_GIT" diff --cached --name-status >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached empty file matches git --numstat' '
	(
	cd repo &&
	grit diff --cached --numstat >grit_out &&
	"$REAL_GIT" diff --cached --numstat >git_out &&
	test_cmp git_out grit_out
	)
'

test_done
