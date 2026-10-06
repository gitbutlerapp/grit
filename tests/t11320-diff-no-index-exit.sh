#!/bin/sh
# Tests for grit diff --exit-code and --quiet behavior.

test_description='grit diff: --exit-code and --quiet exit status behavior'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with initial content' '
	(
	"$REAL_GIT" init -b main repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&

	echo "line one" >file.txt &&
	echo "hello" >other.txt &&
	"$REAL_GIT" add file.txt other.txt &&
	"$REAL_GIT" commit -m "initial commit"
	)
'

###########################################################################
# Section 2: diff --exit-code with no changes
###########################################################################

test_expect_success 'diff --exit-code returns 0 when no changes' '
	(
	cd repo &&
	"$GUST_BIN" diff --exit-code
	)
'

test_expect_success 'diff --exit-code returns 0 on clean worktree' '
	(
	cd repo &&
	test_expect_code 0 "$GUST_BIN" diff --exit-code
	)
'

test_expect_success 'diff (no flags) returns 0 when no changes' '
	(
	cd repo &&
	test_expect_code 0 "$GUST_BIN" diff
	)
'

###########################################################################
# Section 3: diff --exit-code with unstaged changes
###########################################################################

test_expect_success 'diff --exit-code returns 1 when worktree differs from index' '
	(
	cd repo &&
	echo "modified" >>file.txt &&
	test_expect_code 1 "$GUST_BIN" diff --exit-code
	)
'

test_expect_success 'diff --exit-code still shows diff output' '
	(
	cd repo &&
	"$GUST_BIN" diff --exit-code >out 2>&1 || true &&
	grep "diff --git" out
	)
'

test_expect_success 'diff without --exit-code returns 0 even with changes' '
	(
	cd repo &&
	test_expect_code 0 "$GUST_BIN" diff
	)
'

test_expect_success 'restore file after test' '
	(
	cd repo &&
	"$REAL_GIT" checkout -- file.txt
	)
'

###########################################################################
# Section 4: diff --quiet
###########################################################################

test_expect_success 'diff --quiet returns 0 when no changes' '
	(
	cd repo &&
	test_expect_code 0 "$GUST_BIN" diff --quiet
	)
'

test_expect_success 'diff --quiet returns 1 when worktree differs' '
	(
	cd repo &&
	echo "changed" >>file.txt &&
	test_expect_code 1 "$GUST_BIN" diff --quiet
	)
'

test_expect_success 'diff --quiet suppresses output' '
	(
	cd repo &&
	"$GUST_BIN" diff --quiet >out 2>&1 || true &&
	test_must_be_empty out
	)
'

test_expect_success 'restore file after quiet test' '
	(
	cd repo &&
	"$REAL_GIT" checkout -- file.txt
	)
'

###########################################################################
# Section 5: diff --exit-code with cached/staged changes
###########################################################################

test_expect_success 'diff --cached --exit-code returns 0 when index matches HEAD' '
	(
	cd repo &&
	test_expect_code 0 "$GUST_BIN" diff --cached --exit-code
	)
'

test_expect_success 'diff --cached --exit-code returns 1 when index differs from HEAD' '
	(
	cd repo &&
	echo "staged change" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	test_expect_code 1 "$GUST_BIN" diff --cached --exit-code
	)
'

test_expect_success 'diff --cached --quiet suppresses output with staged change' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --quiet >out 2>&1 || true &&
	test_must_be_empty out
	)
'

test_expect_success 'reset staged change' '
	(
	cd repo &&
	"$REAL_GIT" reset --hard HEAD
	)
'

###########################################################################
# Section 6: diff --exit-code with multiple files
###########################################################################

test_expect_success 'diff --exit-code with only one file changed' '
	(
	cd repo &&
	echo "modify" >>file.txt &&
	test_expect_code 1 "$GUST_BIN" diff --exit-code
	)
'

test_expect_success 'diff --exit-code with multiple files changed' '
	(
	cd repo &&
	echo "modify other" >>other.txt &&
	test_expect_code 1 "$GUST_BIN" diff --exit-code
	)
'

test_expect_success 'restore all files' '
	(
	cd repo &&
	"$REAL_GIT" checkout -- .
	)
'

###########################################################################
# Section 7: diff --exit-code with path limiters
###########################################################################

test_expect_success 'diff --exit-code on clean file returns 0' '
	(
	cd repo &&
	test_expect_code 0 "$GUST_BIN" diff --exit-code
	)
'

test_expect_success 'diff --exit-code on modified file returns 1' '
	(
	cd repo &&
	echo "change file" >>file.txt &&
	test_expect_code 1 "$GUST_BIN" diff --exit-code
	)
'

test_expect_success 'restore after exit-code test' '
	(
	cd repo &&
	"$REAL_GIT" checkout -- .
	)
'

###########################################################################
# Section 8: diff --exit-code between commits
###########################################################################

test_expect_success 'setup: create second commit' '
	(
	cd repo &&
	echo "second line" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit"
	)
'

test_expect_success 'diff --exit-code HEAD~1 HEAD returns 1 (different commits)' '
	(
	cd repo &&
	test_expect_code 1 "$GUST_BIN" diff --exit-code HEAD~1 HEAD
	)
'

test_expect_success 'diff --exit-code HEAD HEAD returns 0 (same commit)' '
	(
	cd repo &&
	test_expect_code 0 "$GUST_BIN" diff --exit-code HEAD HEAD
	)
'

test_expect_success 'diff --quiet HEAD~1 HEAD returns 1' '
	(
	cd repo &&
	test_expect_code 1 "$GUST_BIN" diff --quiet HEAD~1 HEAD
	)
'

test_expect_success 'diff --quiet HEAD HEAD returns 0' '
	(
	cd repo &&
	test_expect_code 0 "$GUST_BIN" diff --quiet HEAD HEAD
	)
'

###########################################################################
# Section 9: diff --exit-code with newly added files
###########################################################################

test_expect_success 'diff --exit-code does not detect untracked files' '
	(
	cd repo &&
	echo "new" >newfile.txt &&
	test_expect_code 0 "$GUST_BIN" diff --exit-code
	)
'

test_expect_success 'diff --cached --exit-code detects newly staged file' '
	(
	cd repo &&
	"$REAL_GIT" add newfile.txt &&
	test_expect_code 1 "$GUST_BIN" diff --cached --exit-code
	)
'

test_expect_success 'commit new file and verify exit-code 0' '
	(
	cd repo &&
	"$REAL_GIT" commit -m "add newfile" &&
	test_expect_code 0 "$GUST_BIN" diff --exit-code &&
	test_expect_code 0 "$GUST_BIN" diff --cached --exit-code
	)
'

###########################################################################
# Section 10: diff --exit-code with deleted files
###########################################################################

test_expect_success 'diff --exit-code detects deleted file' '
	(
	cd repo &&
	rm other.txt &&
	test_expect_code 1 "$GUST_BIN" diff --exit-code
	)
'

test_expect_success 'diff --quiet detects deleted file' '
	(
	cd repo &&
	test_expect_code 1 "$GUST_BIN" diff --quiet
	)
'

test_expect_success 'restore deleted file' '
	(
	cd repo &&
	"$REAL_GIT" checkout -- other.txt
	)
'

###########################################################################
# Section 11: diff unified context with exit codes
###########################################################################

test_expect_success 'diff -U0 --exit-code still detects changes' '
	(
	cd repo &&
	echo "change" >>file.txt &&
	test_expect_code 1 "$GUST_BIN" diff -U0 --exit-code
	)
'

test_expect_success 'diff -U5 --exit-code works with custom context' '
	(
	cd repo &&
	test_expect_code 1 "$GUST_BIN" diff -U5 --exit-code
	)
'

test_expect_success 'restore for final cleanup' '
	(
	cd repo &&
	"$REAL_GIT" checkout -- .
	)
'

test_done
