#!/bin/sh
# Tests for diff options: --stat, --numstat, --name-only, --name-status,
# --cached, --exit-code, --quiet, -U/--unified.

test_description='diff output format options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ──────────────────────────────────────────────────────────────

test_expect_success 'setup repo with initial commit' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	printf "line 1\nline 2\nline 3\n" >file.txt &&
	echo "other content" >other.txt &&
	grit add . &&
	grit commit -m "initial"
	)
'

# ── Worktree diff: --exit-code and --quiet ─────────────────────────────

test_expect_success 'diff --exit-code returns 0 on clean worktree' '
	(
	cd repo &&
	grit diff --exit-code
	)
'

test_expect_success 'diff --exit-code returns 1 when worktree differs' '
	(
	cd repo &&
	echo "line 4" >>file.txt &&
	test_must_fail grit diff --exit-code
	)
'

test_expect_success 'diff --quiet returns 1 when worktree differs' '
	(
	cd repo &&
	test_must_fail grit diff --quiet
	)
'

test_expect_success 'diff --quiet produces no output' '
	(
	cd repo &&
	grit diff --quiet >out 2>&1 || true &&
	test_line_count = 0 out
	)
'

# ── Worktree diff: --name-only, --name-status, --stat, --numstat ──────

test_expect_success 'diff --name-only lists changed files' '
	(
	cd repo &&
	grit diff --name-only >actual &&
	grep "file.txt" actual &&
	! grep "other.txt" actual
	)
'

test_expect_success 'diff --name-status shows M for modified' '
	(
	cd repo &&
	grit diff --name-status >actual &&
	grep "^M" actual &&
	grep "file.txt" actual
	)
'

test_expect_success 'diff --name-only with multiple changes' '
	(
	cd repo &&
	echo "changed" >>other.txt &&
	grit diff --name-only >actual &&
	grep "file.txt" actual &&
	grep "other.txt" actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'diff --stat shows file summary' '
	(
	cd repo &&
	grit diff --stat >actual &&
	grep "file.txt" actual &&
	grep "other.txt" actual
	)
'

test_expect_success 'diff --numstat shows numeric counts' '
	(
	cd repo &&
	grit diff --numstat >actual &&
	grep "file.txt" actual &&
	grep "other.txt" actual
	)
'

# ── Stage changes for --cached tests ──────────────────────────────────

test_expect_success 'stage all changes' '
	(
	cd repo &&
	grit add .
	)
'

test_expect_success 'diff (worktree) is empty after staging' '
	(
	cd repo &&
	grit diff --exit-code
	)
'

test_expect_success 'diff --cached shows staged changes' '
	(
	cd repo &&
	grit diff --cached >actual &&
	grep "file.txt" actual
	)
'

test_expect_success 'diff --cached patch has correct + lines' '
	(
	cd repo &&
	grit diff --cached >actual &&
	grep "^+line 4" actual
	)
'

test_expect_success 'diff --cached --stat works' '
	(
	cd repo &&
	grit diff --cached --stat >actual &&
	grep "file.txt" actual
	)
'

test_expect_success 'diff --cached --numstat works' '
	(
	cd repo &&
	grit diff --cached --numstat >actual &&
	grep "file.txt" actual
	)
'

test_expect_success 'diff --cached --name-only works' '
	(
	cd repo &&
	grit diff --cached --name-only >actual &&
	grep "file.txt" actual &&
	grep "other.txt" actual
	)
'

test_expect_success 'diff --cached --name-status works' '
	(
	cd repo &&
	grit diff --cached --name-status >actual &&
	grep "^M" actual
	)
'

test_expect_success 'diff --cached --exit-code returns 1' '
	(
	cd repo &&
	test_must_fail grit diff --cached --exit-code
	)
'

test_expect_success 'diff --cached --quiet produces no output but exits 1' '
	(
	cd repo &&
	grit diff --cached --quiet >out 2>&1 || true &&
	test_line_count = 0 out &&
	test_must_fail grit diff --cached --quiet
	)
'

# ── -U / --unified context ────────────────────────────────────────────

test_expect_success 'diff --cached -U0 shows hunks with no context' '
	(
	cd repo &&
	grit diff --cached -U0 >actual &&
	grep "^@@" actual
	)
'

test_expect_success 'diff --cached -U1 shows hunks with 1 context line' '
	(
	cd repo &&
	grit diff --cached -U1 >actual &&
	grep "^@@" actual
	)
'

# ── After commit, diffs are empty ─────────────────────────────────────

test_expect_success 'commit clears cached diff' '
	(
	cd repo &&
	grit commit -m "second" &&
	grit diff --cached >cached_diff &&
	test_line_count = 0 cached_diff
	)
'

# ── New file: staged diff ─────────────────────────────────────────────

test_expect_success 'diff --cached shows new file' '
	(
	cd repo &&
	echo "brand new" >newfile.txt &&
	grit add newfile.txt &&
	grit diff --cached --name-status >actual &&
	grep "^A" actual &&
	grep "newfile.txt" actual
	)
'

test_expect_success 'diff --cached --stat for new file' '
	(
	cd repo &&
	grit diff --cached --stat >actual &&
	grep "newfile.txt" actual
	)
'

# ── Deleted file: staged diff ─────────────────────────────────────────

test_expect_success 'diff --cached shows deleted file' '
	(
	cd repo &&
	grit rm other.txt &&
	grit diff --cached --name-status >actual &&
	grep "^D" actual &&
	grep "other.txt" actual
	)
'

test_done
