#!/bin/sh
# Comprehensive tests for diff-files: comparing worktree against index.

test_description='diff-files comprehensive worktree tests'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

SYS_GIT=$(command -v git)

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup: repository with committed files' '
	(
	"$SYS_GIT" init repo &&
	cd repo &&
	"$SYS_GIT" config user.name "Test User" &&
	"$SYS_GIT" config user.email "test@example.com" &&
	echo "hello" >file1.txt &&
	echo "world" >file2.txt &&
	echo "constant" >unchanged.txt &&
	mkdir -p sub/deep &&
	echo "nested" >sub/nested.txt &&
	echo "deep" >sub/deep/bottom.txt &&
	"$SYS_GIT" add . &&
	"$SYS_GIT" commit -m "initial"
	)
'

# ── Clean worktree ───────────────────────────────────────────────────────

test_expect_success 'diff-files: clean worktree produces empty output' '
	(
	cd repo &&
	git diff-files >../df_out &&
	test_must_be_empty ../df_out
	)
'

# ── Single file modification ────────────────────────────────────────────

test_expect_success 'setup: modify one file in worktree' '
	(
	cd repo &&
	echo "modified hello" >file1.txt
	)
'

test_expect_success 'diff-files: shows modified file' '
	(
	cd repo &&
	git diff-files >../df_out &&
	grep "file1.txt" ../df_out
	)
'

test_expect_success 'diff-files: modification shows M status' '
	(
	cd repo &&
	git diff-files >../df_out &&
	grep "M" ../df_out | grep "file1.txt"
	)
'

test_expect_success 'diff-files: does not show unchanged files' '
	(
	cd repo &&
	git diff-files >../df_out &&
	! grep "unchanged.txt" ../df_out &&
	! grep "file2.txt" ../df_out
	)
'

test_expect_success 'diff-files: raw format has colon prefix' '
	(
	cd repo &&
	git diff-files >../df_out &&
	grep "^:" ../df_out
	)
'

test_expect_success 'diff-files: raw format has modes and OIDs' '
	(
	cd repo &&
	git diff-files >../df_out &&
	grep -E "^:[0-9]{6} [0-9]{6} [0-9a-f]{40}" ../df_out
	)
'

test_expect_success 'diff-files: worktree oid is null (not computed)' '
	(
	cd repo &&
	git diff-files >../df_out &&
	grep "0000000000000000000000000000000000000000" ../df_out
	)
'

# ── --name-only ──────────────────────────────────────────────────────────

test_expect_success 'diff-files --name-only: shows only filename' '
	(
	cd repo &&
	git diff-files --name-only >../df_out &&
	grep "file1.txt" ../df_out &&
	! grep "^:" ../df_out
	)
'

# ── --name-status ────────────────────────────────────────────────────────

test_expect_success 'diff-files --name-status: shows M and filename' '
	(
	cd repo &&
	git diff-files --name-status >../df_out &&
	grep "^M" ../df_out | grep "file1.txt"
	)
'

# ── -p (patch) ───────────────────────────────────────────────────────────

test_expect_success 'diff-files -p: shows diff header' '
	(
	cd repo &&
	git diff-files -p >../df_out &&
	grep "^diff --git" ../df_out
	)
'

test_expect_success 'diff-files -p: shows hunk header' '
	(
	cd repo &&
	git diff-files -p >../df_out &&
	grep "^@@" ../df_out
	)
'

test_expect_success 'diff-files -p: shows old and new content' '
	(
	cd repo &&
	git diff-files -p >../df_out &&
	grep "^-hello" ../df_out &&
	grep "^+modified hello" ../df_out
	)
'

# ── --stat ───────────────────────────────────────────────────────────────

test_expect_success 'diff-files --stat: shows stat output' '
	(
	cd repo &&
	git diff-files --stat >../df_out &&
	grep "file1.txt" ../df_out &&
	grep "changed" ../df_out
	)
'

# ── Multiple modified files ──────────────────────────────────────────────

test_expect_success 'setup: modify second file' '
	(
	cd repo &&
	echo "modified world" >file2.txt
	)
'

test_expect_success 'diff-files: shows both modified files' '
	(
	cd repo &&
	git diff-files >../df_out &&
	grep "file1.txt" ../df_out &&
	grep "file2.txt" ../df_out
	)
'

test_expect_success 'diff-files --name-only: lists both files' '
	(
	cd repo &&
	git diff-files --name-only >../df_out &&
	grep "file1.txt" ../df_out &&
	grep "file2.txt" ../df_out
	)
'

test_expect_success 'diff-files --stat: shows both in stat' '
	(
	cd repo &&
	git diff-files --stat >../df_out &&
	grep "file1.txt" ../df_out &&
	grep "file2.txt" ../df_out
	)
'

# ── Subdirectory modification ────────────────────────────────────────────

test_expect_success 'setup: modify file in subdirectory' '
	(
	cd repo &&
	"$SYS_GIT" checkout -- file1.txt file2.txt &&
	echo "updated nested" >sub/nested.txt
	)
'

test_expect_success 'diff-files: shows subdirectory path' '
	(
	cd repo &&
	git diff-files >../df_out &&
	grep "sub/nested.txt" ../df_out
	)
'

test_expect_success 'diff-files --name-only: shows subdirectory path' '
	(
	cd repo &&
	git diff-files --name-only >../df_out &&
	grep "sub/nested.txt" ../df_out
	)
'

test_expect_success 'diff-files -p: shows patch for subdirectory file' '
	(
	cd repo &&
	git diff-files -p >../df_out &&
	grep "^diff --git a/sub/nested.txt b/sub/nested.txt" ../df_out
	)
'

# ── Deep subdirectory ────────────────────────────────────────────────────

test_expect_success 'setup: modify deeply nested file' '
	(
	cd repo &&
	"$SYS_GIT" checkout -- sub/nested.txt &&
	echo "updated deep" >sub/deep/bottom.txt
	)
'

test_expect_success 'diff-files: shows deeply nested path' '
	(
	cd repo &&
	git diff-files >../df_out &&
	grep "sub/deep/bottom.txt" ../df_out
	)
'

# ── After staging: file disappears from diff-files ───────────────────────

test_expect_success 'setup: stage the modification' '
	(
	cd repo &&
	"$SYS_GIT" add sub/deep/bottom.txt
	)
'

test_expect_success 'diff-files: staged file disappears from output' '
	(
	cd repo &&
	git diff-files >../df_out &&
	! grep "sub/deep/bottom.txt" ../df_out
	)
'

# ── New untracked files do NOT appear ────────────────────────────────────

test_expect_success 'setup: create untracked file' '
	(
	cd repo &&
	echo "untracked" >newuntracked.txt
	)
'

test_expect_success 'diff-files: untracked files do not appear' '
	(
	cd repo &&
	git diff-files >../df_out &&
	! grep "newuntracked.txt" ../df_out
	)
'

# ── Deleted worktree file ───────────────────────────────────────────────

test_expect_success 'setup: delete a tracked file from worktree' '
	(
	cd repo &&
	"$SYS_GIT" commit -m "stage deep" &&
	rm -f file2.txt
	)
'

test_expect_success 'diff-files: deleted file shows D status' '
	(
	cd repo &&
	git diff-files >../df_out &&
	grep "D" ../df_out | grep "file2.txt"
	)
'

test_expect_success 'diff-files --name-status: deleted file shows D' '
	(
	cd repo &&
	git diff-files --name-status >../df_out &&
	grep "^D" ../df_out | grep "file2.txt"
	)
'

test_expect_success 'diff-files -p: deleted file shows removed content' '
	(
	cd repo &&
	git diff-files -p >../df_out &&
	grep "^-world" ../df_out
	)
'

# ── Restore and verify clean ────────────────────────────────────────────

test_expect_success 'setup: restore deleted file' '
	(
	cd repo &&
	"$SYS_GIT" reset --hard HEAD
	)
'

test_expect_success 'diff-files: clean after restore' '
	(
	cd repo &&
	git diff-files >../df_out &&
	test_must_be_empty ../df_out
	)
'

test_done
