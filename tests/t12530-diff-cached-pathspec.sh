#!/bin/sh
# Tests for grit diff --cached across various staging scenarios:
# partial staging, new files, deletions, mode changes, empty diffs,
# and combinations with output format flags.

test_description='grit diff --cached: staging scenarios'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

test_expect_success 'setup: repo with multiple files' '
	(
	grit init repo &&
	cd repo &&
	"$REAL_GIT" config user.email "t@t.com" &&
	"$REAL_GIT" config user.name "T" &&
	echo "alpha" >a.txt &&
	echo "bravo" >b.txt &&
	echo "charlie" >c.txt &&
	mkdir -p dir &&
	echo "deep" >dir/d.txt &&
	echo "extra" >dir/e.txt &&
	grit add . &&
	grit commit -m "initial"
	)
'

# ---- clean state ----

test_expect_success 'diff --cached: clean state produces no output' '
	(cd repo && grit diff --cached >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff --cached: clean state matches git' '
	(cd repo && grit diff --cached >../grit_out &&
	 "$REAL_GIT" diff --cached >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff --cached --exit-code: returns 0 on clean state' '
	(cd repo && grit diff --cached --exit-code)
'

# ---- single file staged modification ----

test_expect_success 'setup: stage modification to a.txt' '
	(cd repo && echo "alpha2" >a.txt && grit add a.txt)
'

test_expect_success 'diff --cached: single staged mod shows patch' '
	(cd repo && grit diff --cached >../actual) &&
	grep "a.txt" actual &&
	grep "alpha2" actual
'

test_expect_success 'diff --cached: single staged mod matches git' '
	(cd repo && grit diff --cached >../grit_out &&
	 "$REAL_GIT" diff --cached >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff --cached --name-only: single file' '
	(cd repo && grit diff --cached --name-only >../actual) &&
	echo "a.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached --name-status: single file shows M' '
	(cd repo && grit diff --cached --name-status >../actual) &&
	printf "M\ta.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached --numstat: single file shows numbers' '
	(cd repo && grit diff --cached --numstat >../grit_out &&
	 "$REAL_GIT" diff --cached --numstat >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff --cached --exit-code: returns 1 with staged changes' '
	(cd repo && test_must_fail grit diff --cached --exit-code)
'

test_expect_success 'diff --cached --quiet: suppresses output' '
	(cd repo && test_must_fail grit diff --cached --quiet >../actual) &&
	test_must_be_empty actual
'

# ---- multiple files staged ----

test_expect_success 'setup: stage multiple modifications' '
	(cd repo &&
	 echo "bravo2" >b.txt &&
	 echo "deep2" >dir/d.txt &&
	 grit add b.txt dir/d.txt)
'

test_expect_success 'diff --cached: multiple files matches git' '
	(cd repo && grit diff --cached >../grit_out &&
	 "$REAL_GIT" diff --cached >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff --cached --name-only: lists all staged files' '
	(cd repo && grit diff --cached --name-only >../actual) &&
	grep "a.txt" actual &&
	grep "b.txt" actual &&
	grep "dir/d.txt" actual
'

test_expect_success 'diff --cached --name-only: no unstaged files appear' '
	(cd repo &&
	 echo "charlie2" >c.txt &&
	 grit diff --cached --name-only >../actual) &&
	! grep "c.txt" actual
'

test_expect_success 'diff --cached --stat: shows summary' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "3 files changed" actual
'

test_expect_success 'diff --cached --stat: matches git' '
	(cd repo && grit diff --cached --stat >../grit_out &&
	 "$REAL_GIT" diff --cached --stat >../git_out) &&
	test_cmp git_out grit_out
'

# ---- staged new file (addition) ----

test_expect_success 'setup: commit current and add new file' '
	(cd repo &&
	 "$REAL_GIT" checkout -- c.txt &&
	 grit commit -m "second" &&
	 echo "new content" >new.txt &&
	 grit add new.txt)
'

test_expect_success 'diff --cached: new file shows as addition' '
	(cd repo && grit diff --cached >../actual) &&
	grep "new file" actual &&
	grep "new.txt" actual
'

test_expect_success 'diff --cached --name-status: new file shows A' '
	(cd repo && grit diff --cached --name-status >../actual) &&
	grep "^A" actual | grep "new.txt"
'

test_expect_success 'diff --cached: new file matches git' '
	(cd repo && grit diff --cached >../grit_out &&
	 "$REAL_GIT" diff --cached >../git_out) &&
	test_cmp git_out grit_out
'

# ---- staged deletion ----

test_expect_success 'setup: stage deletion' '
	(cd repo &&
	 "$REAL_GIT" rm c.txt)
'

test_expect_success 'diff --cached: deletion shows deleted file mode' '
	(cd repo && grit diff --cached >../actual) &&
	grep "deleted file" actual &&
	grep "c.txt" actual
'

test_expect_success 'diff --cached --name-status: deletion shows D' '
	(cd repo && grit diff --cached --name-status >../actual) &&
	grep "^D" actual | grep "c.txt"
'

test_expect_success 'diff --cached --name-status: both A and D present' '
	(cd repo && grit diff --cached --name-status >../actual) &&
	grep "^A" actual &&
	grep "^D" actual
'

test_expect_success 'diff --cached: add+delete matches git' '
	(cd repo && grit diff --cached >../grit_out &&
	 "$REAL_GIT" diff --cached >../git_out) &&
	test_cmp git_out grit_out
'

# ---- context lines (-U) ----

test_expect_success 'setup: commit and create multi-line file change' '
	(cd repo &&
	 grit commit -m "third" &&
	 for i in 1 2 3 4 5 6 7 8 9 10; do echo "line$i"; done >a.txt &&
	 grit add a.txt)
'

test_expect_success 'diff --cached -U0: zero context lines' '
	(cd repo && grit diff --cached -U0 >../grit_out &&
	 "$REAL_GIT" diff --cached -U0 >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff --cached -U1: one context line' '
	(cd repo && grit diff --cached -U1 >../grit_out &&
	 "$REAL_GIT" diff --cached -U1 >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff --cached -U10: large context matches git' '
	(cd repo && grit diff --cached -U10 >../grit_out &&
	 "$REAL_GIT" diff --cached -U10 >../git_out) &&
	test_cmp git_out grit_out
'

# ---- mixed staged and unstaged ----

test_expect_success 'diff --cached: ignores unstaged worktree changes' '
	(cd repo &&
	 echo "unstaged change" >b.txt &&
	 grit diff --cached --name-only >../actual) &&
	grep "a.txt" actual &&
	! grep "b.txt" actual
'

test_expect_success 'diff (no --cached): shows unstaged only' '
	(cd repo && grit diff --name-only >../actual) &&
	grep "b.txt" actual &&
	! grep "a.txt" actual
'

# ---- staged changes to nested directory files ----

test_expect_success 'setup: stage nested file changes' '
	(cd repo &&
	 "$REAL_GIT" checkout -- b.txt &&
	 echo "extra2" >dir/e.txt &&
	 echo "deep3" >dir/d.txt &&
	 grit add dir/)
'

test_expect_success 'diff --cached --name-only: nested files appear' '
	(cd repo && grit diff --cached --name-only >../actual) &&
	grep "dir/d.txt" actual &&
	grep "dir/e.txt" actual
'

test_expect_success 'diff --cached: nested changes match git' '
	(cd repo && grit diff --cached >../grit_out &&
	 "$REAL_GIT" diff --cached >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff --cached --numstat: nested files match git' '
	(cd repo && grit diff --cached --numstat >../grit_out &&
	 "$REAL_GIT" diff --cached --numstat >../git_out) &&
	test_cmp git_out grit_out
'

test_done
