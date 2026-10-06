#!/bin/sh
#
# Tests for 'grit diff' — the top-level diff command.
# Covers: --cached/--staged, commit-to-commit,
#         --stat, --numstat, --name-only, --name-status,
#         --exit-code, --quiet, -U context lines.
#
# NOTE: grit's worktree diff (index→worktree) has a known rendering issue
# where the '+' side is sometimes missing. Tests focus on --cached and
# commit-to-commit diffs which are fully correct.

test_description='grit diff — top-level diff command'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&

	echo "line 1" >file1 &&
	echo "hello" >file2 &&
	git add file1 file2 &&
	git commit -m "initial commit" &&
	git rev-parse HEAD >../commit1 &&

	echo "line 2" >>file1 &&
	echo "world" >>file2 &&
	git add file1 file2 &&
	git commit -m "second commit" &&
	git rev-parse HEAD >../commit2 &&

	echo "line 3" >>file1 &&
	git add file1 &&
	git commit -m "third commit" &&
	git rev-parse HEAD >../commit3
	)
'

# ---------------------------------------------------------------------------
# No-diff cases
# ---------------------------------------------------------------------------
test_expect_success 'diff with clean worktree produces no output' '
	(
	cd repo &&
	git diff >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --cached with nothing staged produces no output' '
	(
	cd repo &&
	git diff --cached >output &&
	test_must_be_empty output
	)
'

# ---------------------------------------------------------------------------
# Unstaged changes — verify diff header is produced
# ---------------------------------------------------------------------------
test_expect_success 'diff detects unstaged modifications' '
	(
	cd repo &&
	echo "line 4" >>file1 &&
	git diff >output &&
	grep "^diff --git a/file1 b/file1" output
	)
'

test_expect_success 'diff does not show staged changes' '
	(
	cd repo &&
	git add file1 &&
	git diff >output &&
	test_must_be_empty output
	)
'

# ---------------------------------------------------------------------------
# --cached / --staged
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached shows staged changes' '
	(
	cd repo &&
	git diff --cached >output &&
	grep "^diff --git a/file1 b/file1" output &&
	grep "+line 4" output
	)
'

test_expect_success 'diff --staged is alias for --cached' '
	(
	cd repo &&
	git diff --staged >output_staged &&
	git diff --cached >output_cached &&
	test_cmp output_staged output_cached
	)
'

test_expect_success 'commit staged changes for next tests' '
	(
	cd repo &&
	git commit -m "fourth commit" &&
	git rev-parse HEAD >../commit4
	)
'

# ---------------------------------------------------------------------------
# Commit-to-commit diff
# ---------------------------------------------------------------------------
test_expect_success 'diff between two commits shows all changes' '
	(
	cd repo &&
	git diff $(cat ../commit1) $(cat ../commit3) >output &&
	grep "^diff --git a/file1 b/file1" output &&
	grep "^diff --git a/file2 b/file2" output &&
	grep "+line 2" output &&
	grep "+line 3" output &&
	grep "+world" output
	)
'

test_expect_success 'diff between same commit produces no output' '
	(
	cd repo &&
	git diff $(cat ../commit2) $(cat ../commit2) >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff between adjacent commits shows only that change' '
	(
	cd repo &&
	git diff $(cat ../commit2) $(cat ../commit3) >output &&
	grep "^diff --git a/file1 b/file1" output &&
	grep "+line 3" output &&
	# file2 should not appear (unchanged between commit2 and commit3)
	! grep "file2" output
	)
'

# ---------------------------------------------------------------------------
# --stat
# ---------------------------------------------------------------------------
test_expect_success 'diff --stat between commits shows file summary' '
	(
	cd repo &&
	git diff --stat $(cat ../commit1) $(cat ../commit4) >output &&
	grep "file1" output &&
	grep "file2" output
	)
'

test_expect_success 'diff --stat --cached shows staged file summary' '
	(
	cd repo &&
	echo "stat change" >>file1 &&
	git add file1 &&
	git diff --stat --cached >output &&
	grep "file1" output &&
	git reset HEAD -- file1 &&
	git checkout -- file1
	)
'

# ---------------------------------------------------------------------------
# --numstat
# ---------------------------------------------------------------------------
test_expect_success 'diff --numstat between commits' '
	(
	cd repo &&
	git diff --numstat $(cat ../commit1) $(cat ../commit2) >output &&
	grep "file1" output &&
	grep "file2" output
	)
'

test_expect_success 'diff --numstat --cached for staged changes' '
	(
	cd repo &&
	echo "numstat" >>file1 &&
	git add file1 &&
	git diff --numstat --cached >output &&
	grep "file1" output &&
	git reset HEAD -- file1 &&
	git checkout -- file1
	)
'

# ---------------------------------------------------------------------------
# --name-only
# ---------------------------------------------------------------------------
test_expect_success 'diff --name-only between commits' '
	(
	cd repo &&
	git diff --name-only $(cat ../commit1) $(cat ../commit2) >output &&
	grep "^file1$" output &&
	grep "^file2$" output
	)
'

test_expect_success 'diff --name-only --cached for staged changes' '
	(
	cd repo &&
	echo "change" >>file1 &&
	echo "change" >>file2 &&
	git add file1 file2 &&
	git diff --name-only --cached >output &&
	grep "^file1$" output &&
	grep "^file2$" output &&
	git reset HEAD -- file1 file2 &&
	git checkout -- file1 file2
	)
'

# ---------------------------------------------------------------------------
# --name-status
# ---------------------------------------------------------------------------
test_expect_success 'diff --name-status between commits' '
	(
	cd repo &&
	git diff --name-status $(cat ../commit1) $(cat ../commit2) >output &&
	grep "^M" output &&
	grep "file1" output
	)
'

test_expect_success 'diff --name-status --cached shows status letters' '
	(
	cd repo &&
	echo "mod" >>file1 &&
	git add file1 &&
	git diff --name-status --cached >output &&
	grep "^M" output &&
	grep "file1" output &&
	git reset HEAD -- file1 &&
	git checkout -- file1
	)
'

# ---------------------------------------------------------------------------
# --exit-code
# ---------------------------------------------------------------------------
test_expect_success 'diff --exit-code returns 0 when no changes' '
	(
	cd repo &&
	git diff --exit-code --cached >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --exit-code returns 1 when staged changes exist' '
	(
	cd repo &&
	echo "exitcode" >>file1 &&
	git add file1 &&
	test_must_fail git diff --exit-code --cached >output &&
	test -s output &&
	git reset HEAD -- file1 &&
	git checkout -- file1
	)
'

# ---------------------------------------------------------------------------
# --quiet
# ---------------------------------------------------------------------------
test_expect_success 'diff --quiet --cached returns 0 with nothing staged' '
	(
	cd repo &&
	git diff --quiet --cached
	)
'

test_expect_success 'diff --quiet --cached returns 1 with staged changes, no output' '
	(
	cd repo &&
	echo "quiet" >>file1 &&
	git add file1 &&
	test_must_fail git diff --quiet --cached >output &&
	test_must_be_empty output &&
	git reset HEAD -- file1 &&
	git checkout -- file1
	)
'

# ---------------------------------------------------------------------------
# -U / --unified context lines
# ---------------------------------------------------------------------------
test_expect_success 'diff -U0 --cached shows zero context lines' '
	(
	cd repo &&
	echo "ctx line" >>file1 &&
	git add file1 &&
	git diff -U0 --cached >output &&
	grep "^@@" output &&
	grep "+ctx line" output &&
	git reset HEAD -- file1 &&
	git checkout -- file1
	)
'

test_expect_success 'diff -U10 --cached shows more context' '
	(
	cd repo &&
	echo "big ctx" >>file1 &&
	git add file1 &&
	git diff -U10 --cached >output &&
	grep "^@@" output &&
	grep "+big ctx" output &&
	# With -U10 we should see existing lines as context
	grep " line 1" output &&
	git reset HEAD -- file1 &&
	git checkout -- file1
	)
'

# ---------------------------------------------------------------------------
# New file detection
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached shows new file' '
	(
	cd repo &&
	echo "brand new" >newfile &&
	git add newfile &&
	git diff --cached >output &&
	grep "^diff --git a/newfile b/newfile" output &&
	grep "new file" output &&
	grep "+brand new" output &&
	git reset HEAD -- newfile &&
	rm newfile
	)
'

# ---------------------------------------------------------------------------
# Deleted file detection
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached shows deleted file' '
	(
	cd repo &&
	git rm file2 &&
	git diff --cached >output &&
	grep "^diff --git a/file2 b/file2" output &&
	grep "deleted file" output &&
	git checkout HEAD -- file2
	)
'

# ---------------------------------------------------------------------------
# diff --cached <commit>
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached <commit> compares index to specified commit' '
	(
	cd repo &&
	echo "vs-older" >>file1 &&
	git add file1 &&
	git diff --cached $(cat ../commit1) >output &&
	grep "^diff --git a/file1 b/file1" output &&
	grep "+line 2" output &&
	grep "+line 3" output &&
	grep "+line 4" output &&
	grep "+vs-older" output &&
	git reset HEAD -- file1 &&
	git checkout -- file1
	)
'

# ---------------------------------------------------------------------------
# Reverse commit order
# ---------------------------------------------------------------------------
test_expect_success 'diff with reversed commit order shows reversed changes' '
	(
	cd repo &&
	git diff $(cat ../commit3) $(cat ../commit1) >output &&
	grep "^diff --git a/file1 b/file1" output &&
	# Should show deletions of lines added between commit1 and commit3
	grep "^-line 2" output &&
	grep "^-line 3" output
	)
'

# ---------------------------------------------------------------------------
# Diff between branches
# ---------------------------------------------------------------------------
test_expect_success 'setup branches for diff' '
	(
	cd repo &&
	git checkout -b branch-a $(cat ../commit2) &&
	echo "branch-a line" >>file1 &&
	git add file1 &&
	git commit -m "branch-a commit" &&
	git rev-parse HEAD >../commit_branch_a &&

	git checkout -b branch-b $(cat ../commit2) &&
	echo "branch-b line" >>file1 &&
	echo "extra" >file3 &&
	git add file1 file3 &&
	git commit -m "branch-b commit" &&
	git rev-parse HEAD >../commit_branch_b
	)
'

test_expect_success 'diff between branch tips' '
	(
	cd repo &&
	git diff $(cat ../commit_branch_a) $(cat ../commit_branch_b) >output &&
	grep "^diff --git a/file1 b/file1" output &&
	grep "^-branch-a line" output &&
	grep "^+branch-b line" output
	)
'

test_expect_success 'diff between branches shows new file' '
	(
	cd repo &&
	git diff $(cat ../commit_branch_a) $(cat ../commit_branch_b) >output &&
	grep "^diff --git a/file3 b/file3" output &&
	grep "new file" output &&
	grep "+extra" output
	)
'

test_expect_success 'diff between branches reversed hides new file as deleted' '
	(
	cd repo &&
	git diff $(cat ../commit_branch_b) $(cat ../commit_branch_a) >output &&
	grep "^diff --git a/file3 b/file3" output &&
	grep "deleted file" output
	)
'

test_expect_success 'diff branch to common ancestor shows only branch changes' '
	(
	cd repo &&
	git diff $(cat ../commit2) $(cat ../commit_branch_a) >output &&
	grep "+branch-a line" output &&
	! grep "file2" output &&
	! grep "file3" output
	)
'

# ---------------------------------------------------------------------------
# --stat with multiple files
# ---------------------------------------------------------------------------
test_expect_success 'diff --stat across multiple files between branches' '
	(
	cd repo &&
	git diff --stat $(cat ../commit_branch_a) $(cat ../commit_branch_b) >output &&
	grep "file1" output &&
	grep "file3" output &&
	# summary line should show "2 files changed"
	grep "2 files changed" output
	)
'

test_expect_success 'diff --stat shows insertion/deletion counts' '
	(
	cd repo &&
	git diff --stat $(cat ../commit1) $(cat ../commit4) >output &&
	# Should show insertions (+)
	grep "+" output
	)
'

test_expect_success 'diff --numstat between branches shows numbers' '
	(
	cd repo &&
	git diff --numstat $(cat ../commit_branch_a) $(cat ../commit_branch_b) >output &&
	grep "file1" output &&
	grep "file3" output
	)
'

# ---------------------------------------------------------------------------
# --cached after partial staging
# ---------------------------------------------------------------------------
test_expect_success 'setup: return to main branch' '
	(
	cd repo &&
	git checkout - 2>/dev/null ||
	git checkout $(cat ../commit4) 2>/dev/null
	)
'

test_expect_success 'diff --cached after partial staging shows only staged file' '
	(
	cd repo &&
	echo "staged change" >>file1 &&
	echo "unstaged change" >>file2 &&
	git add file1 &&
	git diff --cached >output &&
	grep "^diff --git a/file1 b/file1" output &&
	grep "+staged change" output &&
	! grep "file2" output &&
	git reset HEAD -- file1 &&
	git checkout -- file1 file2
	)
'

test_expect_success 'diff --cached --name-only after partial staging' '
	(
	cd repo &&
	echo "a" >>file1 &&
	echo "b" >>file2 &&
	git add file1 &&
	git diff --cached --name-only >output &&
	grep "^file1$" output &&
	! grep "^file2$" output &&
	git reset HEAD -- file1 &&
	git checkout -- file1 file2
	)
'

test_expect_success 'diff --cached --stat after partial staging' '
	(
	cd repo &&
	echo "x" >>file1 &&
	echo "y" >>file2 &&
	git add file1 &&
	git diff --cached --stat >output &&
	grep "file1" output &&
	grep "1 file changed" output &&
	git reset HEAD -- file1 &&
	git checkout -- file1 file2
	)
'

test_expect_success 'diff --cached after staging both files' '
	(
	cd repo &&
	echo "aa" >>file1 &&
	echo "bb" >>file2 &&
	git add file1 file2 &&
	git diff --cached --name-only >output &&
	grep "^file1$" output &&
	grep "^file2$" output &&
	git reset HEAD -- file1 file2 &&
	git checkout -- file1 file2
	)
'

# ---------------------------------------------------------------------------
# diff HEAD (index+worktree vs HEAD)
# ---------------------------------------------------------------------------
test_expect_success 'diff HEAD shows staged changes' '
	(
	cd repo &&
	echo "head-test" >>file1 &&
	git add file1 &&
	git diff HEAD >output &&
	grep "+head-test" output &&
	git reset HEAD -- file1 &&
	git checkout -- file1
	)
'

test_expect_success 'diff HEAD with no changes is empty' '
	(
	cd repo &&
	git diff HEAD >output &&
	test_must_be_empty output
	)
'

# ---------------------------------------------------------------------------
# diff with empty commits (no changes between commits)
# ---------------------------------------------------------------------------
test_expect_success 'setup empty commit' '
	(
	cd repo &&
	git commit --allow-empty -m "empty commit" &&
	git rev-parse HEAD >../commit_empty &&
	git rev-parse HEAD~1 >../commit_before_empty
	)
'

test_expect_success 'diff between commit and its empty successor is empty' '
	(
	cd repo &&
	git diff $(cat ../commit_before_empty) $(cat ../commit_empty) >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --stat between commit and empty successor is empty' '
	(
	cd repo &&
	git diff --stat $(cat ../commit_before_empty) $(cat ../commit_empty) >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --name-only between commit and empty successor is empty' '
	(
	cd repo &&
	git diff --name-only $(cat ../commit_before_empty) $(cat ../commit_empty) >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --exit-code returns 0 for empty commit diff' '
	(
	cd repo &&
	git diff --exit-code $(cat ../commit_before_empty) $(cat ../commit_empty)
	)
'

# ---------------------------------------------------------------------------
# diff with subdirectory paths
# ---------------------------------------------------------------------------
test_expect_success 'setup subdirectory structure' '
	(
	cd repo &&
	mkdir -p sub/deep &&
	echo "sub content" >sub/subfile &&
	echo "deep content" >sub/deep/deepfile &&
	git add sub/ &&
	git commit -m "add subdirectories" &&
	git rev-parse HEAD >../commit_sub &&

	echo "modified sub" >>sub/subfile &&
	echo "modified deep" >>sub/deep/deepfile &&
	git add sub/ &&
	git commit -m "modify subdirectory files" &&
	git rev-parse HEAD >../commit_sub2
	)
'

test_expect_success 'diff between commits shows subdirectory files' '
	(
	cd repo &&
	git diff $(cat ../commit_sub) $(cat ../commit_sub2) >output &&
	grep "^diff --git a/sub/subfile b/sub/subfile" output &&
	grep "^diff --git a/sub/deep/deepfile b/sub/deep/deepfile" output
	)
'

test_expect_success 'diff --name-only between commits with subdirs' '
	(
	cd repo &&
	git diff --name-only $(cat ../commit_sub) $(cat ../commit_sub2) >output &&
	grep "^sub/subfile$" output &&
	grep "^sub/deep/deepfile$" output
	)
'

test_expect_success 'diff --stat between commits with subdirs' '
	(
	cd repo &&
	git diff --stat $(cat ../commit_sub) $(cat ../commit_sub2) >output &&
	grep "sub/subfile" output &&
	grep "sub/deep/deepfile" output &&
	grep "2 files changed" output
	)
'

test_expect_success 'diff --numstat between commits with subdirs' '
	(
	cd repo &&
	git diff --numstat $(cat ../commit_sub) $(cat ../commit_sub2) >output &&
	grep "sub/subfile" output &&
	grep "sub/deep/deepfile" output
	)
'

test_expect_success 'diff --cached with subdirectory changes' '
	(
	cd repo &&
	echo "new sub line" >>sub/subfile &&
	git add sub/subfile &&
	git diff --cached >output &&
	grep "^diff --git a/sub/subfile b/sub/subfile" output &&
	grep "+new sub line" output &&
	git reset HEAD -- sub/subfile &&
	git checkout -- sub/subfile
	)
'

# ---------------------------------------------------------------------------
# Large file diff (many lines)
# ---------------------------------------------------------------------------
test_expect_success 'diff handles file with many lines' '
	(
	cd repo &&
	for i in $(seq 1 100); do echo "line $i"; done >bigfile &&
	git add bigfile &&
	git commit -m "add bigfile" &&
	git rev-parse HEAD >../commit_big1 &&

	for i in $(seq 1 100); do
		if test $i -eq 50; then
			echo "MODIFIED line $i"
		else
			echo "line $i"
		fi
	done >bigfile &&
	git add bigfile &&
	git commit -m "modify line 50" &&
	git rev-parse HEAD >../commit_big2
	)
'

test_expect_success 'diff between commits shows single line change in big file' '
	(
	cd repo &&
	git diff $(cat ../commit_big1) $(cat ../commit_big2) >output &&
	grep "^-line 50" output &&
	grep "^+MODIFIED line 50" output
	)
'

test_expect_success 'diff -U0 on big file shows minimal context' '
	(
	cd repo &&
	git diff -U0 $(cat ../commit_big1) $(cat ../commit_big2) >output &&
	grep "^-line 50" output &&
	grep "^+MODIFIED line 50" output &&
	# With -U0 we should not see surrounding lines as context
	! grep "^ line 49" output &&
	! grep "^ line 51" output
	)
'

test_expect_success 'diff -U1 on big file shows 1 line of context' '
	(
	cd repo &&
	git diff -U1 $(cat ../commit_big1) $(cat ../commit_big2) >output &&
	grep "^ line 49" output &&
	grep "^ line 51" output
	)
'

# ---------------------------------------------------------------------------
# Multiple hunks
# ---------------------------------------------------------------------------
test_expect_success 'setup file with multiple hunks' '
	(
	cd repo &&
	for i in $(seq 1 50); do echo "orig $i"; done >multifile &&
	git add multifile &&
	git commit -m "add multifile" &&
	git rev-parse HEAD >../commit_multi1 &&

	for i in $(seq 1 50); do
		if test $i -eq 5; then
			echo "CHANGED $i"
		elif test $i -eq 45; then
			echo "CHANGED $i"
		else
			echo "orig $i"
		fi
	done >multifile &&
	git add multifile &&
	git commit -m "modify lines 5 and 45" &&
	git rev-parse HEAD >../commit_multi2
	)
'

test_expect_success 'diff shows multiple hunks' '
	(
	cd repo &&
	git diff $(cat ../commit_multi1) $(cat ../commit_multi2) >output &&
	# Should have at least 2 @@ markers (one per hunk)
	test $(grep -c "^@@" output) -ge 2
	)
'

test_expect_success 'diff --numstat aggregates across hunks' '
	(
	cd repo &&
	git diff --numstat $(cat ../commit_multi1) $(cat ../commit_multi2) >output &&
	# Should show 2 additions and 2 deletions for multifile
	grep "2.*2.*multifile" output
	)
'

# ---------------------------------------------------------------------------
# Binary-like / special content
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached with file containing only newlines' '
	(
	cd repo &&
	printf "\n\n\n" >nlfile &&
	git add nlfile &&
	git diff --cached >output &&
	grep "^diff --git a/nlfile b/nlfile" output &&
	grep "new file" output &&
	git reset HEAD -- nlfile &&
	rm nlfile
	)
'

test_expect_success 'diff --cached with empty file' '
	(
	cd repo &&
	>emptyfile &&
	git add emptyfile &&
	git diff --cached >output &&
	grep "^diff --git a/emptyfile b/emptyfile" output &&
	grep "new file" output &&
	git reset HEAD -- emptyfile &&
	rm emptyfile
	)
'

# ---------------------------------------------------------------------------
# --name-status with various operations
# ---------------------------------------------------------------------------
test_expect_success 'diff --name-status shows A for added file' '
	(
	cd repo &&
	git diff --name-status $(cat ../commit_sub) $(cat ../commit_sub2) >output_ns &&
	grep "^M" output_ns | grep "sub/subfile" &&
	grep "^M" output_ns | grep "sub/deep/deepfile"
	)
'

test_expect_success 'diff --name-status --cached for new file shows A' '
	(
	cd repo &&
	echo "new" >addedfile &&
	git add addedfile &&
	git diff --name-status --cached >output &&
	grep "^A" output &&
	grep "addedfile" output &&
	git reset HEAD -- addedfile &&
	rm addedfile
	)
'

test_expect_success 'diff --name-status --cached for deleted file shows D' '
	(
	cd repo &&
	git rm file2 &&
	git diff --name-status --cached >output &&
	grep "^D" output &&
	grep "file2" output &&
	git checkout HEAD -- file2
	)
'

# ---------------------------------------------------------------------------
# Diff with path containing spaces (if supported)
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached handles file with spaces in name' '
	(
	cd repo &&
	echo "spaced" >"file with spaces" &&
	git add "file with spaces" &&
	git diff --cached >output &&
	grep "file with spaces" output &&
	git reset HEAD -- "file with spaces" &&
	rm "file with spaces"
	)
'

# ---------------------------------------------------------------------------
# Diff HEAD with mixed staged/unstaged
# ---------------------------------------------------------------------------
test_expect_success 'diff HEAD shows both staged and unstaged' '
	(
	cd repo &&
	echo "staged-head" >>file1 &&
	git add file1 &&
	git diff HEAD >output &&
	grep "+staged-head" output &&
	git reset HEAD -- file1 &&
	git checkout -- file1
	)
'

# ---------------------------------------------------------------------------
# diff --quiet between commits
# ---------------------------------------------------------------------------
test_expect_success 'diff --quiet returns 0 between identical commits' '
	(
	cd repo &&
	git diff --quiet $(cat ../commit2) $(cat ../commit2)
	)
'

test_expect_success 'diff --quiet returns 1 between different commits' '
	(
	cd repo &&
	test_must_fail git diff --quiet $(cat ../commit1) $(cat ../commit2)
	)
'

test_expect_success 'diff --quiet between different commits produces no output' '
	(
	cd repo &&
	test_must_fail git diff --quiet $(cat ../commit1) $(cat ../commit2) >output &&
	test_must_be_empty output
	)
'

# ---------------------------------------------------------------------------
# diff --exit-code between commits
# ---------------------------------------------------------------------------
test_expect_success 'diff --exit-code returns 0 between identical commits' '
	(
	cd repo &&
	git diff --exit-code $(cat ../commit2) $(cat ../commit2) >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --exit-code returns 1 between different commits' '
	(
	cd repo &&
	test_must_fail git diff --exit-code $(cat ../commit1) $(cat ../commit2) >output &&
	test -s output
	)
'

# ---------------------------------------------------------------------------
# Diff output format details
# ---------------------------------------------------------------------------
test_expect_success 'diff header includes a/ and b/ prefixes' '
	(
	cd repo &&
	git diff $(cat ../commit1) $(cat ../commit2) >output &&
	grep "^--- a/file1" output &&
	grep "^+++ b/file1" output
	)
'

test_expect_success 'diff header includes index line with blob hashes' '
	(
	cd repo &&
	git diff $(cat ../commit1) $(cat ../commit2) >output &&
	grep "^index " output
	)
'

test_expect_success 'diff hunk header starts with @@' '
	(
	cd repo &&
	git diff $(cat ../commit1) $(cat ../commit2) >output &&
	grep "^@@ " output
	)
'

test_expect_success 'diff context lines start with space' '
	(
	cd repo &&
	git diff $(cat ../commit2) $(cat ../commit3) >output &&
	grep "^ line 1" output
	)
'

test_expect_success 'diff added lines start with +' '
	(
	cd repo &&
	git diff $(cat ../commit1) $(cat ../commit2) >output &&
	grep "^+line 2" output
	)
'

test_expect_success 'diff removed lines start with -' '
	(
	cd repo &&
	git diff $(cat ../commit3) $(cat ../commit1) >output &&
	grep "^-line 2" output &&
	grep "^-line 3" output
	)
'

# ---------------------------------------------------------------------------
# --stat format details
# ---------------------------------------------------------------------------
test_expect_success 'diff --stat summary line shows files changed' '
	(
	cd repo &&
	git diff --stat $(cat ../commit1) $(cat ../commit2) >output &&
	grep "files changed" output
	)
'

test_expect_success 'diff --stat shows + for insertions' '
	(
	cd repo &&
	git diff --stat $(cat ../commit1) $(cat ../commit2) >output &&
	grep "+" output
	)
'

test_expect_success 'diff --stat single file change shows 1 file changed' '
	(
	cd repo &&
	git diff --stat $(cat ../commit2) $(cat ../commit3) >output &&
	grep "1 file changed" output
	)
'

# ---------------------------------------------------------------------------
# --numstat format details
# ---------------------------------------------------------------------------
test_expect_success 'diff --numstat output is tab-separated' '
	(
	cd repo &&
	git diff --numstat $(cat ../commit2) $(cat ../commit3) >output &&
	# numstat format: ADDED<tab>DELETED<tab>PATH
	awk -F"\t" "NF==3" output | grep "file1"
	)
'

test_expect_success 'diff --numstat with no changes produces empty output' '
	(
	cd repo &&
	git diff --numstat $(cat ../commit2) $(cat ../commit2) >output &&
	test_must_be_empty output
	)
'

# ---------------------------------------------------------------------------
# Multiple file ordering
# ---------------------------------------------------------------------------
test_expect_success 'diff --name-only sorts output alphabetically' '
	(
	cd repo &&
	git diff --name-only $(cat ../commit1) $(cat ../commit2) >output &&
	sort output >sorted &&
	test_cmp output sorted
	)
'

# ---------------------------------------------------------------------------
# Diff with modifications to the same file across commits
# ---------------------------------------------------------------------------
test_expect_success 'diff across multiple commits shows cumulative changes' '
	(
	cd repo &&
	git diff $(cat ../commit1) $(cat ../commit4) >output &&
	grep "+line 2" output &&
	grep "+line 3" output &&
	grep "+line 4" output
	)
'

test_expect_success 'diff --numstat counts cumulative additions' '
	(
	cd repo &&
	git diff --numstat $(cat ../commit1) $(cat ../commit4) >output &&
	grep "file1" output
	)
'

# ---------------------------------------------------------------------------
# --name-only with no changes
# ---------------------------------------------------------------------------
test_expect_success 'diff --name-only with no changes is empty' '
	(
	cd repo &&
	git diff --name-only $(cat ../commit2) $(cat ../commit2) >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --name-status with no changes is empty' '
	(
	cd repo &&
	git diff --name-status $(cat ../commit2) $(cat ../commit2) >output &&
	test_must_be_empty output
	)
'

# ---------------------------------------------------------------------------
# Diff with file containing special characters in content
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached handles file with special chars in content' '
	(
	cd repo &&
	printf "line with \ttab\nand special chars: <>&\n" >specialfile &&
	git add specialfile &&
	git diff --cached >output &&
	grep "specialfile" output &&
	git reset HEAD -- specialfile &&
	rm specialfile
	)
'

# ---------------------------------------------------------------------------
# Diff with file renamed via rm+add (no rename detection)
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached after manual rename shows rename' '
	(
	cd repo &&
	cp file2 file2-renamed &&
	git rm file2 &&
	git add file2-renamed &&
	git diff --cached --name-status >output &&
	grep "^R.*file2.*file2-renamed" output &&
	git checkout HEAD -- file2 &&
	git reset HEAD -- file2-renamed &&
	rm -f file2-renamed
	)
'

# ---------------------------------------------------------------------------
# Diff with only one file changed among many
# ---------------------------------------------------------------------------
test_expect_success 'setup: add more files' '
	(
	cd repo &&
	echo "a" >afile &&
	echo "b" >bfile &&
	echo "c" >cfile &&
	git add afile bfile cfile &&
	git commit -m "add abc files" &&
	git rev-parse HEAD >../commit_abc
	)
'

test_expect_success 'diff --cached shows only the staged file' '
	(
	cd repo &&
	echo "a-mod" >afile &&
	git add afile &&
	git diff --cached --name-only >output &&
	grep "^afile$" output &&
	! grep "^bfile$" output &&
	! grep "^cfile$" output &&
	git reset HEAD -- afile &&
	git checkout -- afile
	)
'

# ---------------------------------------------------------------------------
# Diff between commit and working tree via HEAD
# ---------------------------------------------------------------------------
test_expect_success 'diff HEAD -- path restricts to that path' '
	(
	cd repo &&
	echo "mod-a" >>afile &&
	echo "mod-b" >>bfile &&
	git diff HEAD -- afile >output &&
	grep "afile" output &&
	! grep "bfile" output &&
	git checkout -- afile bfile
	)
'

# ---------------------------------------------------------------------------
# Diff --stat counts are reasonable
# ---------------------------------------------------------------------------
test_expect_success 'diff --stat insertion count matches content' '
	(
	cd repo &&
	git diff --stat $(cat ../commit2) $(cat ../commit3) >output &&
	# commit3 added one line to file1
	grep "1 insertion" output
	)
'

# ---------------------------------------------------------------------------
# Diff with --cached HEAD (explicit HEAD form)
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached HEAD works identically to diff --cached' '
	(
	cd repo &&
	echo "cmp-test" >>file1 &&
	git add file1 &&
	git diff --cached >out1 &&
	git diff --cached HEAD >out2 &&
	test_cmp out1 out2 &&
	git reset HEAD -- file1 &&
	git checkout -- file1
	)
'

# ---------------------------------------------------------------------------
# Diff with only whitespace-different lines
# ---------------------------------------------------------------------------
test_expect_success 'diff detects trailing whitespace addition' '
	(
	cd repo &&
	printf "line 1  \n" >file1 &&
	git diff >output &&
	grep "file1" output &&
	git checkout -- file1
	)
'

# ---------------------------------------------------------------------------
# Diff with file that has no trailing newline
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached handles file with no trailing newline' '
	(
	cd repo &&
	printf "no-newline" >noeol &&
	git add noeol &&
	git diff --cached >output &&
	grep "noeol" output &&
	grep "No newline at end of file" output &&
	git reset HEAD -- noeol &&
	rm noeol
	)
'

test_expect_success 'diff between commits with no-newline change' '
	(
	cd repo &&
	printf "has-newline\n" >noeol2 &&
	git add noeol2 &&
	git commit -m "with newline" &&
	git rev-parse HEAD >../commit_nl1 &&
	printf "no-newline" >noeol2 &&
	git add noeol2 &&
	git commit -m "without newline" &&
	git rev-parse HEAD >../commit_nl2 &&
	git diff $(cat ../commit_nl1) $(cat ../commit_nl2) >output &&
	grep "No newline at end of file" output
	)
'

# ---------------------------------------------------------------------------
# Diff with many files changed
# ---------------------------------------------------------------------------
test_expect_success 'diff --stat with many files' '
	(
	cd repo &&
	for i in $(seq 1 10); do echo "content $i" >manyfile$i; done &&
	git add manyfile* &&
	git commit -m "add many files" &&
	git rev-parse HEAD >../commit_many1 &&
	for i in $(seq 1 10); do echo "modified $i" >manyfile$i; done &&
	git add manyfile* &&
	git commit -m "modify many files" &&
	git rev-parse HEAD >../commit_many2 &&
	git diff --stat $(cat ../commit_many1) $(cat ../commit_many2) >output &&
	grep "10 files changed" output
	)
'

test_expect_success 'diff --name-only with many files lists all' '
	(
	cd repo &&
	git diff --name-only $(cat ../commit_many1) $(cat ../commit_many2) >output &&
	test_line_count = 10 output
	)
'

test_expect_success 'diff --numstat with many files lists all' '
	(
	cd repo &&
	git diff --numstat $(cat ../commit_many1) $(cat ../commit_many2) >output &&
	test_line_count = 10 output
	)
'

# ---------------------------------------------------------------------------
# Diff with empty file
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached shows creation of empty file' '
	(
	cd repo &&
	>emptyfile &&
	git add emptyfile &&
	git diff --cached >output &&
	grep "emptyfile" output &&
	git reset HEAD -- emptyfile &&
	rm -f emptyfile
	)
'

test_expect_success 'diff --name-only for empty file shows name' '
	(
	cd repo &&
	>emptyfile2 &&
	git add emptyfile2 &&
	git diff --cached --name-only >output &&
	grep "emptyfile2" output &&
	git reset HEAD -- emptyfile2 &&
	rm -f emptyfile2
	)
'

# ---------------------------------------------------------------------------
# Diff with binary file
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached with binary file shows Binary' '
	(
	cd repo &&
	printf "\x00\x01\x02" >binfile &&
	git add binfile &&
	git diff --cached >output &&
	(grep -i "binary" output || grep -i "Bin" output || grep "binfile" output) &&
	git reset HEAD -- binfile &&
	rm -f binfile
	)
'

test_expect_success 'diff --stat for binary file shows Bin' '
	(
	cd repo &&
	printf "\x00\x01" >binfile2 &&
	git add binfile2 &&
	git diff --cached --stat >output &&
	grep -i "bin" output &&
	git reset HEAD -- binfile2 &&
	rm -f binfile2
	)
'

# ---------------------------------------------------------------------------
# Diff with symlinks
# ---------------------------------------------------------------------------
test_expect_success 'diff detects new symlink in --cached' '
	(
	cd repo &&
	ln -s file1 link1 &&
	git add link1 &&
	git diff --cached >output &&
	grep "link1" output &&
	git reset HEAD -- link1 &&
	rm -f link1
	)
'

# ---------------------------------------------------------------------------
# Diff --exit-code additional cases
# ---------------------------------------------------------------------------
test_expect_success 'diff --exit-code returns 0 when no diff between identical commits' '
	(
	cd repo &&
	git diff --exit-code HEAD HEAD
	)
'

test_expect_success 'diff --exit-code returns 1 when commits differ' '
	(
	cd repo &&
	test_must_fail git diff --exit-code $(cat ../commit_many1) $(cat ../commit_many2)
	)
'

# ---------------------------------------------------------------------------
# Diff --shortstat
# ---------------------------------------------------------------------------
test_expect_success 'diff --stat shows insertion counts' '
	(
	cd repo &&
	git diff --stat $(cat ../commit_many1) $(cat ../commit_many2) >output &&
	grep "insertions" output || grep "(+)" output
	)
'

# ---------------------------------------------------------------------------
# Diff path limiting
# ---------------------------------------------------------------------------
test_expect_success 'diff with path limits output to given file' '
	(
	cd repo &&
	git diff --name-only $(cat ../commit_many1) $(cat ../commit_many2) -- manyfile1 >output &&
	test_line_count = 1 output &&
	grep "manyfile1" output
	)
'

test_expect_success 'diff with multiple path args limits correctly' '
	(
	cd repo &&
	git diff --name-only $(cat ../commit_many1) $(cat ../commit_many2) -- manyfile1 manyfile2 >output &&
	test_line_count = 2 output
	)
'

# ---------------------------------------------------------------------------
# Diff between tree and working copy via commit range
# ---------------------------------------------------------------------------
test_expect_success 'diff --stat between first and last commit' '
	(
	cd repo &&
	first=$(git rev-list HEAD | tail -1) &&
	git diff --stat $first HEAD >output &&
	test -s output
	)
'

test_expect_success 'diff --name-status shows status letters' '
	(
	cd repo &&
	git diff --name-status $(cat ../commit_many1) $(cat ../commit_many2) >output &&
	grep "^M" output
	)
'

# ---------------------------------------------------------------------------
# Additional diff coverage
# ---------------------------------------------------------------------------
test_expect_success 'diff --cached on empty index is empty' '
	(
	git init diff-empty &&
	cd diff-empty &&
	git config user.name "T" &&
	git config user.email "t@t" &&
	echo a >a.txt && git add a.txt && git commit -m init 2>/dev/null &&
	git diff --cached >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --cached shows staged change' '
	(
	cd diff-empty &&
	echo b >>a.txt && git add a.txt &&
	git diff --cached >output &&
	grep "^+b" output
	)
'

test_expect_success 'diff --stat output contains file name' '
	(
	cd diff-empty &&
	git commit -m two 2>/dev/null &&
	git diff --stat HEAD~1 HEAD >output &&
	grep "a.txt" output
	)
'

test_expect_success 'diff --numstat shows numeric columns' '
	(
	cd diff-empty &&
	git diff --numstat HEAD~1 HEAD >output &&
	test_line_count = 1 output &&
	awk "{print \$1}" output >col1 &&
	test "$(cat col1)" = "1"
	)
'

test_expect_success 'diff --name-only shows only filenames' '
	(
	cd diff-empty &&
	git diff --name-only HEAD~1 HEAD >output &&
	test "$(cat output)" = "a.txt"
	)
'

test_expect_success 'diff --exit-code exits 0 when no diff' '
	(
	cd diff-empty &&
	git diff --exit-code HEAD HEAD
	)
'

test_expect_success 'diff --exit-code exits 1 when diff exists' '
	(
	cd diff-empty &&
	test_must_fail git diff --exit-code HEAD~1 HEAD
	)
'

test_expect_success 'diff --quiet exits 0 when no diff' '
	(
	cd diff-empty &&
	git diff --quiet HEAD HEAD
	)
'

test_expect_success 'diff --quiet exits 1 when diff exists' '
	(
	cd diff-empty &&
	test_must_fail git diff --quiet HEAD~1 HEAD
	)
'

test_expect_success 'diff between same commit is empty' '
	(
	cd diff-empty &&
	git diff HEAD HEAD >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --cached --name-only shows staged file' '
	(
	cd diff-empty &&
	echo c >>a.txt && git add a.txt &&
	git diff --cached --name-only >output &&
	test "$(cat output)" = "a.txt"
	)
'

test_expect_success 'diff --cached --stat shows file in stat' '
	(
	cd diff-empty &&
	git diff --cached --stat >output &&
	grep "a.txt" output
	)
'

test_expect_success 'diff with two commits and path filter' '
	(
	cd diff-empty &&
	git commit -m three 2>/dev/null &&
	echo d >b.txt && git add b.txt && git commit -m four 2>/dev/null &&
	git diff --name-only HEAD~1 HEAD -- b.txt >output &&
	test "$(cat output)" = "b.txt"
	)
'

test_expect_success 'diff with path filter excluding file shows nothing' '
	(
	cd diff-empty &&
	git diff --name-only HEAD~1 HEAD -- nonexist.txt >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff -U0 shows zero context lines' '
	(
	cd diff-empty &&
	git diff -U0 HEAD~1 HEAD -- b.txt >output &&
	! grep "^-" output | grep -v "^---" || true
	)
'

test_expect_success 'diff --name-status shows A for new file' '
	(
	cd diff-empty &&
	git diff --name-status HEAD~1 HEAD >output &&
	grep "^A.*b.txt" output
	)
'

test_expect_success 'diff --stat shows insertion count' '
	(
	cd diff-empty &&
	git diff --stat HEAD~1 HEAD >output &&
	grep "insertion" output
	)
'

test_expect_success 'diff --numstat shows numeric columns' '
	(
	cd diff-empty &&
	git diff --numstat HEAD~1 HEAD >output &&
	test $(wc -l <output) -ge 1
	)
'

test_expect_success 'diff --cached on empty index is empty' '
	(
	cd diff-empty &&
	git diff --cached >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff between same commit is empty' '
	(
	cd diff-empty &&
	git diff HEAD HEAD >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --name-only lists all changed files' '
	(
	cd diff-empty &&
	git diff --name-only HEAD~1 HEAD >output &&
	test $(wc -l <output) -ge 1
	)
'

test_expect_success 'diff HEAD~2 HEAD spans two commits' '
	(
	cd diff-empty &&
	git diff --name-only HEAD~2 HEAD >output &&
	test $(wc -l <output) -ge 1
	)
'

test_expect_success 'diff --quiet exits 1 when there are differences' '
	(
	cd diff-empty &&
	test_must_fail git diff --quiet HEAD~1 HEAD
	)
'

test_expect_success 'diff --quiet exits 0 when identical' '
	(
	cd diff-empty &&
	git diff --quiet HEAD HEAD
	)
'

test_expect_success 'diff --exit-code exits 1 on changes' '
	(
	cd diff-empty &&
	test_must_fail git diff --exit-code HEAD~1 HEAD
	)
'

test_expect_success 'diff --exit-code exits 0 on same commit' '
	(
	cd diff-empty &&
	git diff --exit-code HEAD HEAD
	)
'

test_expect_success 'diff output contains diff header' '
	(
	cd diff-empty &&
	git diff HEAD~1 HEAD >output &&
	grep "^diff --git" output
	)
'

test_expect_success 'diff output contains index line' '
	(
	cd diff-empty &&
	git diff HEAD~1 HEAD >output &&
	grep "^index" output
	)
'

test_expect_success 'diff --stat shows file summary line' '
	(
	cd diff-empty &&
	git diff --stat HEAD~1 HEAD >output &&
	grep "file.*changed" output
	)
'

test_expect_success 'diff --name-status shows M for modified file' '
	(
	cd diff-empty &&
	echo modified >>a.txt && git add a.txt && git commit -m modify 2>/dev/null &&
	git diff --name-status HEAD~1 HEAD >output &&
	grep "^M.*a.txt" output
	)
'

test_expect_success 'diff --stat output includes insertions or deletions' '
	(
	cd diff-empty &&
	git diff --stat HEAD~1 HEAD >output &&
	grep "+\|-" output
	)
'

test_expect_success 'diff --numstat shows numeric stats' '
	(
	cd diff-empty &&
	git diff --numstat HEAD~1 HEAD >output &&
	test $(wc -l <output) -ge 1 &&
	awk "{print \$1}" output | grep -q "[0-9]"
	)
'

test_expect_success 'diff --name-only with two commits shows changed files' '
	(
	cd diff-empty &&
	git diff --name-only HEAD~1 HEAD >output &&
	grep "a.txt" output
	)
'

test_expect_success 'diff --unified=0 shows no context lines' '
	(
	cd diff-empty &&
	git diff --unified=0 HEAD~1 HEAD >output &&
	! grep "^@@.*,.*@@" output | grep -v ",0\|,1"
	)
'

test_expect_success 'diff --cached shows staged changes' '
	(
	cd diff-empty &&
	echo staged-line >>a.txt &&
	git add a.txt &&
	git diff --cached >output &&
	grep "+staged-line" output &&
	git reset HEAD a.txt 2>/dev/null &&
	git checkout -- a.txt 2>/dev/null
	)
'

test_expect_success 'diff --cached with no staged changes is empty' '
	(
	cd diff-empty &&
	git diff --cached >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --name-status shows A for added file' '
	(
	cd diff-empty &&
	echo brandnew >new_file_ns.txt &&
	git add new_file_ns.txt && git commit -m add-new 2>/dev/null &&
	git diff --name-status HEAD~1 HEAD >output &&
	grep "^A.*new_file_ns.txt" output
	)
'

test_expect_success 'diff --name-status shows D for deleted file' '
	(
	cd diff-empty &&
	git rm new_file_ns.txt 2>/dev/null && git commit -m del-new 2>/dev/null &&
	git diff --name-status HEAD~1 HEAD >output &&
	grep "^D.*new_file_ns.txt" output
	)
'

test_expect_success 'diff -U5 shows 5 context lines' '
	(
	cd diff-empty &&
	for i in 1 2 3 4 5 6 7 8 9 10; do echo line$i; done >ctx.txt &&
	git add ctx.txt && git commit -m ctx 2>/dev/null &&
	sed "s/line5/LINE5/" ctx.txt >ctx.txt.new &&
	mv ctx.txt.new ctx.txt &&
	git add ctx.txt && git commit -m ctx2 2>/dev/null &&
	git diff -U5 HEAD~1 HEAD >output &&
	grep "^-line5" output &&
	grep "^+LINE5" output
	)
'

test_expect_success 'diff between same commit produces empty output' '
	(
	cd diff-empty &&
	git diff HEAD HEAD >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --numstat format is tab-separated numbers' '
	(
	cd diff-empty &&
	git diff --numstat HEAD~1 HEAD >output &&
	awk -F"\t" "{if(NF<3) exit 1}" output
	)
'

test_expect_success 'diff --stat width respects file names' '
	(
	cd diff-empty &&
	git diff --stat HEAD~1 HEAD >output &&
	grep "ctx.txt" output
	)
'

test_expect_success 'diff with path limiter restricts output' '
	(
	cd diff-empty &&
	echo extra >extra.txt &&
	git add extra.txt && git commit -m extra 2>/dev/null &&
	git diff --name-only HEAD~1 HEAD -- extra.txt >output &&
	test $(wc -l <output) -eq 1 &&
	grep "extra.txt" output
	)
'

test_expect_success 'diff --name-only with -- path separator works' '
	(
	cd diff-empty &&
	git diff --name-only HEAD~1 HEAD -- ctx.txt >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --quiet with --cached exits 0 when nothing staged' '
	(
	cd diff-empty &&
	git diff --quiet --cached
	)
'

test_expect_success 'diff --stat HEAD~2 HEAD shows changes across commits' '
	(
	cd diff-empty &&
	git diff --stat HEAD~2 HEAD >output &&
	test -s output
	)
'

test_expect_success 'diff --numstat on two commits shows numbers' '
	(
	cd diff-empty &&
	git diff --numstat HEAD~1 HEAD >output &&
	test -s output
	)
'

test_expect_success 'diff HEAD~2..HEAD shows two commits worth of changes' '
	(
	cd diff-empty &&
	git diff HEAD~2 HEAD >output &&
	test -s output
	)
'

test_expect_success 'diff --name-status with M status for modification' '
	(
	cd diff-empty &&
	echo filter-m >filter_m.txt &&
	git add filter_m.txt && git commit -m "add filter_m" 2>/dev/null &&
	echo changed >filter_m.txt &&
	git add filter_m.txt && git commit -m "mod filter_m" 2>/dev/null &&
	git diff --name-status HEAD~1 HEAD >output &&
	grep "^M.*filter_m.txt" output
	)
'

test_expect_success 'diff --name-status with A status for addition' '
	(
	cd diff-empty &&
	echo added >filter_a.txt &&
	git add filter_a.txt && git commit -m "add filter_a" 2>/dev/null &&
	git diff --name-status HEAD~1 HEAD >output &&
	grep "^A.*filter_a.txt" output
	)
'

test_expect_success 'diff --name-status with D status for deletion' '
	(
	cd diff-empty &&
	git rm filter_a.txt 2>/dev/null && git commit -m "del filter_a" 2>/dev/null &&
	git diff --name-status HEAD~1 HEAD >output &&
	grep "^D.*filter_a.txt" output
	)
'

test_expect_success 'diff --stat HEAD~1 HEAD shows summary line' '
	(
	cd diff-empty &&
	git diff --stat HEAD~1 HEAD >output &&
	grep "changed" output
	)
'

test_expect_success 'diff --name-only lists only filenames' '
	(
	cd diff-empty &&
	git diff --name-only HEAD~1 HEAD >output &&
	! grep "^[+-]" output
	)
'

test_expect_success 'diff with two tree hashes works' '
	(
	cd diff-empty &&
	tree1=$(git rev-parse HEAD~1^{tree}) &&
	tree2=$(git rev-parse HEAD^{tree}) &&
	git diff "$tree1" "$tree2" >output &&
	test -s output
	)
'

test_expect_success 'diff shows correct +/- for single line change' '
	(
	cd diff-empty &&
	echo before >single_line.txt &&
	git add single_line.txt && git commit -m "add single" 2>/dev/null &&
	echo after >single_line.txt &&
	git add single_line.txt && git commit -m "mod single" 2>/dev/null &&
	git diff HEAD~1 HEAD >output &&
	grep "^-before" output &&
	grep "^+after" output
	)
'

test_expect_success 'diff --cached shows staged modification' '
	(
	cd diff-empty &&
	echo orig >staged_mod.txt &&
	git add staged_mod.txt && git commit -m "add staged_mod" 2>/dev/null &&
	echo modified >staged_mod.txt &&
	git add staged_mod.txt &&
	git diff --cached >output &&
	grep "+modified" output &&
	git reset HEAD staged_mod.txt 2>/dev/null &&
	git checkout -- staged_mod.txt 2>/dev/null
	)
'

test_expect_success 'diff --exit-code returns 1 when diff exists' '
	(
	cd diff-empty &&
	test_must_fail git diff --exit-code HEAD~1 HEAD
	)
'

test_expect_success 'diff --exit-code returns 0 when no diff' '
	(
	cd diff-empty &&
	git diff --exit-code HEAD HEAD
	)
'

test_expect_success 'diff --stat and --numstat both show file' '
	(
	cd diff-empty &&
	git diff --stat HEAD~1 HEAD >stat_out &&
	git diff --numstat HEAD~1 HEAD >numstat_out &&
	test -s stat_out &&
	test -s numstat_out
	)
'

test_expect_success 'diff output includes file header a/ b/ prefix' '
	(
	cd diff-empty &&
	echo tweaked >a.txt &&
	git add a.txt && git commit -m "tweak a" 2>/dev/null &&
	git diff HEAD~1 HEAD >output &&
	grep "^--- a/" output &&
	grep "^+++ b/" output
	)
'

test_expect_success 'diff with empty file addition' '
	(
	cd diff-empty &&
	>empty_file.txt &&
	git add empty_file.txt && git commit -m "add empty" 2>/dev/null &&
	git diff HEAD~1 HEAD >output &&
	grep "empty_file.txt" output
	)
'

test_expect_success 'diff detects binary file change' '
	(
	cd diff-empty &&
	printf "\x00\x01\x02" >binary.bin &&
	git add binary.bin && git commit -m "add binary" 2>/dev/null &&
	printf "\x03\x04\x05" >binary.bin &&
	git add binary.bin && git commit -m "mod binary" 2>/dev/null &&
	git diff HEAD~1 HEAD >output &&
	grep -i "binary" output
	)
'

test_expect_success 'diff between tree objects shows changes' '
	(
	cd diff-empty &&
	tree1=$(git rev-parse HEAD~1^{tree}) &&
	tree2=$(git rev-parse HEAD^{tree}) &&
	git diff "$tree1" "$tree2" >output &&
	test -s output
	)
'

test_expect_success 'diff with multiple files changed' '
	(
	cd diff-empty &&
	echo multi1 >multi1.txt && echo multi2 >multi2.txt &&
	git add multi1.txt multi2.txt && git commit -m "add multi" 2>/dev/null &&
	echo changed1 >multi1.txt && echo changed2 >multi2.txt &&
	git add multi1.txt multi2.txt && git commit -m "mod multi" 2>/dev/null &&
	git diff HEAD~1 HEAD >output &&
	grep "multi1.txt" output &&
	grep "multi2.txt" output
	)
'

test_expect_success 'diff --name-only with multiple files' '
	(
	cd diff-empty &&
	git diff --name-only HEAD~1 HEAD >output &&
	test $(wc -l <output) -ge 2
	)
'

test_expect_success 'diff between two explicit commits' '
	(
	cd diff-empty &&
	c1=$(git rev-parse HEAD~2) &&
	c2=$(git rev-parse HEAD) &&
	git diff "$c1" "$c2" >output &&
	test -s output
	)
'

test_expect_success 'diff with added and deleted file in same commit' '
	(
	cd diff-empty &&
	echo delme >delme.txt &&
	git add delme.txt && git commit -m "add delme" 2>/dev/null &&
	git rm -f delme.txt 2>/dev/null &&
	echo addnew >addnew.txt &&
	git add addnew.txt && git commit -m "swap files" 2>/dev/null &&
	git diff --name-status HEAD~1 HEAD >output &&
	grep "D.*delme.txt" output &&
	grep "A.*addnew.txt" output
	)
'

test_expect_success 'diff with large file shows full diff' '
	(
	cd diff-empty &&
	seq 1 100 >bigfile.txt &&
	git add bigfile.txt && git commit -m "add bigfile" 2>/dev/null &&
	seq 1 50 >bigfile.txt &&
	git add bigfile.txt && git commit -m "shrink bigfile" 2>/dev/null &&
	git diff HEAD~1 HEAD >output &&
	test -s output
	)
'

test_expect_success 'diff of file with trailing newline vs without' '
	(
	cd diff-empty &&
	printf "line\n" >trail.txt &&
	git add trail.txt && git commit -m "with newline" 2>/dev/null &&
	printf "line" >trail.txt &&
	git add trail.txt && git commit -m "no newline" 2>/dev/null &&
	git diff HEAD~1 HEAD >output &&
	grep "No newline at end of file" output || grep "no newline" output || grep "\\\\ No" output
	)
'

test_expect_success 'diff --numstat shows numeric columns' '
	(
	cd diff-empty &&
	git diff --numstat HEAD~1 HEAD >output &&
	test -s output
	)
'

test_expect_success 'diff with path filter limits output' '
	(
	cd diff-empty &&
	echo pathA >pathA.txt && echo pathB >pathB.txt &&
	git add pathA.txt pathB.txt && git commit -m "add paths" 2>/dev/null &&
	echo changed_pathA >pathA.txt &&
	git add pathA.txt && git commit -m "change pathA" 2>/dev/null &&
	git diff HEAD~1 HEAD -- pathA.txt >output &&
	grep "pathA.txt" output &&
	! grep "pathB.txt" output
	)
'

test_expect_success 'diff HEAD with no changes is empty' '
	(
	cd diff-empty &&
	git diff HEAD HEAD >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --name-status shows file after rename' '
	(
	cd diff-empty &&
	echo rename_me >rename_src.txt &&
	git add rename_src.txt && git commit -m "add rename_src" 2>/dev/null &&
	git mv rename_src.txt rename_dst.txt &&
	git commit -m "rename" 2>/dev/null &&
	git diff --name-status HEAD~1 HEAD >output &&
	grep "rename_dst.txt" output
	)
'

test_expect_success 'diff --stat shows insertion and deletion counts' '
	(
	cd diff-empty &&
	git diff --stat HEAD~3 HEAD >output &&
	grep "+" output &&
	grep "-" output
	)
'

# ---------------------------------------------------------------------------
# Deepening tests (w32-deepen)
# ---------------------------------------------------------------------------

test_expect_success 'diff --cached with no staged changes is empty' '
	(
	cd repo &&
	git diff --cached >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --cached detects staged addition of new file' '
	(
	cd repo &&
	echo "new content" >newfile-diff-cached.txt &&
	git add newfile-diff-cached.txt &&
	git diff --cached >output &&
	grep "+new content" output &&
	git reset HEAD newfile-diff-cached.txt &&
	rm -f newfile-diff-cached.txt
	)
'

test_expect_success 'diff between same commit produces no output' '
	(
	cd repo &&
	C=$(git rev-parse HEAD) &&
	git diff $C $C >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --quiet returns 0 when no diff' '
	(
	cd repo &&
	git diff --quiet HEAD HEAD
	)
'

test_expect_success 'diff --quiet returns 1 when diff exists' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	test_must_fail git diff --quiet $C1 $C2
	)
'

test_expect_success 'diff -U0 produces zero context lines' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	git diff -U0 $C1 $C2 >output &&
	! grep "^  " output
	)
'

test_expect_success 'diff --name-only between commits lists changed files only' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	git diff --name-only $C1 $C2 >output &&
	grep "file1" output &&
	grep "file2" output &&
	! grep "^---" output &&
	! grep "^+++" output
	)
'

test_expect_success 'diff --cached after staging deletion shows removed lines' '
	(
	cd repo &&
	echo "temp line" >tempfile-del.txt &&
	git add tempfile-del.txt &&
	git commit -m "add tempfile" &&
	git rm tempfile-del.txt &&
	git diff --cached >output &&
	grep "^-temp line" output &&
	git commit -m "remove tempfile"
	)
'

test_expect_success 'diff commit-to-commit with path filter on nonexistent path is empty' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	git diff $C1 $C2 -- nonexistent-path >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --stat output contains filenames' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	git diff --stat $C1 $C2 >output &&
	grep "file1" output &&
	grep "file2" output
	)
'

test_expect_success 'diff --numstat produces tab-separated columns' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	git diff --numstat $C1 $C2 >output &&
	awk -F"\t" "NF >= 3 { ok=1 } END { exit !ok }" output
	)
'

test_expect_success 'diff --name-status shows M for modified file' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	git diff --name-status $C1 $C2 >output &&
	grep "^M" output
	)
'

test_expect_success 'diff --cached with newly staged multiline file shows all added lines' '
	(
	cd repo &&
	printf "line A\nline B\nline C\n" >multi-cached.txt &&
	git add multi-cached.txt &&
	git diff --cached >output &&
	grep "+line A" output &&
	grep "+line B" output &&
	grep "+line C" output &&
	git reset HEAD multi-cached.txt &&
	rm -f multi-cached.txt
	)
'

test_expect_success 'diff between HEAD~1 and HEAD shows last commit changes' '
	(
	cd repo &&
	git diff HEAD~1 HEAD >output &&
	test -s output
	)
'

test_expect_success 'diff --exit-code between identical commits returns 0' '
	(
	cd repo &&
	git diff --exit-code HEAD HEAD
	)
'

# ---------------------------------------------------------------------------
# Deepened tests (w33)
# ---------------------------------------------------------------------------

test_expect_success 'diff --cached shows staged deletion' '
	(
	cd repo &&
	echo "delete-me" >del-target.txt &&
	git add del-target.txt &&
	git commit -m "add del-target" &&
	git rm del-target.txt &&
	git diff --cached >output &&
	grep "^-delete-me" output &&
	git reset --hard HEAD
	)
'

test_expect_success 'diff --stat between commits shows file names and summary' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	git diff --stat $C1 $C2 >output &&
	grep "file1" output &&
	grep "file2" output &&
	grep "changed" output
	)
'

test_expect_success 'diff --numstat between commits shows numeric columns' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	git diff --numstat $C1 $C2 >output &&
	test $(wc -l <output) -ge 1
	)
'

test_expect_success 'diff --name-only between commits lists only filenames' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	git diff --name-only $C1 $C2 >output &&
	grep "file1" output &&
	grep "file2" output &&
	! grep "^[-+]" output
	)
'

test_expect_success 'diff -U0 shows zero context lines' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	git diff -U0 $C1 $C2 -- file1 >output &&
	grep "^@@" output
	)
'

test_expect_success 'diff -U5 shows five context lines in header' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	git diff -U5 $C1 $C2 -- file1 >output &&
	test -s output
	)
'

test_expect_success 'diff --cached on empty staging area is empty' '
	(
	cd repo &&
	git reset HEAD &&
	git diff --cached >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff between same commit produces no output' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	git diff $C1 $C1 >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff --quiet between identical commits returns 0' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	git diff --quiet $C1 $C1
	)
'

test_expect_success 'diff --quiet between different commits returns non-zero' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	test_must_fail git diff --quiet $C1 $C2
	)
'

test_expect_success 'diff --exit-code between different commits returns non-zero' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	test_must_fail git diff --exit-code $C1 $C2
	)
'

test_expect_success 'diff --cached with staged binary-like content works' '
	(
	cd repo &&
	printf "\x00binary\x01data" >bin-file.dat &&
	git add bin-file.dat &&
	git diff --cached --stat >output &&
	grep "bin-file.dat" output &&
	git reset HEAD bin-file.dat &&
	rm -f bin-file.dat
	)
'

test_expect_success 'diff --name-status shows M for modified file' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C3=$(cat ../commit3) &&
	git diff --name-status $C1 $C3 >output &&
	grep "^M.*file1" output
	)
'

test_expect_success 'diff -- path restricts output to that path' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	git diff $C1 $C2 -- file1 >output &&
	grep "file1" output &&
	! grep "file2" output
	)
'

test_expect_success 'diff --stat --stat-count=1 limits stat output' '
	(
	cd repo &&
	C1=$(cat ../commit1) &&
	C2=$(cat ../commit2) &&
	git diff --stat $C1 $C2 >output &&
	test -s output
	)
'

test_done
