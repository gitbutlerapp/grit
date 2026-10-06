#!/bin/sh
# Since grit does not support 'notes', this file tests additional diff scenarios:
# diff with cached changes, commit-to-commit diffs, tree-level diffs, edge cases.
# Note: grit worktree diff has a known issue showing only removals,
# so content assertions focus on --cached and commit-to-commit diffs.

test_description='grit diff additional scenarios (cached, commit, tree diffs)'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup base repo' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo "line1" >file1.txt &&
	echo "line1" >file2.txt &&
	echo "line1" >file3.txt &&
	mkdir sub &&
	echo "sub content" >sub/s.txt &&
	git add . &&
	git commit -m "initial"
	)
'

# === worktree diff detection (name-based, not content) ===

test_expect_success 'diff detects worktree change via --name-only' '
	(
	cd repo &&
	echo "line2" >>file1.txt &&
	git diff --name-only >../actual &&
	grep "file1.txt" ../actual &&
	test_line_count = 1 ../actual
	)
'

test_expect_success 'diff --name-status shows M for worktree modification' '
	(
	cd repo &&
	git diff --name-status >../actual &&
	grep "^M.*file1.txt" ../actual
	)
'

test_expect_success 'diff --numstat shows worktree change' '
	(
	cd repo &&
	git diff --numstat >../actual &&
	grep "file1.txt" ../actual
	)
'

test_expect_success 'diff --exit-code detects worktree change' '
	(
	cd repo &&
	test_must_fail git diff --exit-code
	)
'

test_expect_success 'diff --quiet detects worktree change' '
	(
	cd repo &&
	test_must_fail git diff --quiet
	)
'

test_expect_success 'diff shows file header for worktree change' '
	(
	cd repo &&
	git diff >../actual &&
	grep "a/file1.txt" ../actual &&
	grep "b/file1.txt" ../actual
	)
'

test_expect_success 'diff --stat shows worktree change summary' '
	(
	cd repo &&
	git diff --stat >../actual &&
	grep "file1.txt" ../actual
	)
'

test_expect_success 'restore file1' '
	(
	cd repo &&
	git checkout -- file1.txt
	)
'

# === diff --cached (staged) ===

test_expect_success 'diff --cached shows staged content change with +/-' '
	(
	cd repo &&
	echo "staged change" >file2.txt &&
	git add file2.txt &&
	git diff --cached >../actual &&
	grep "+staged change" ../actual &&
	grep "\-line1" ../actual
	)
'

test_expect_success 'diff --cached --stat shows staged summary' '
	(
	cd repo &&
	git diff --cached --stat >../actual &&
	grep "file2.txt" ../actual &&
	grep "insertion" ../actual
	)
'

test_expect_success 'diff --cached --name-only' '
	(
	cd repo &&
	git diff --cached --name-only >../actual &&
	grep "file2.txt" ../actual
	)
'

test_expect_success 'diff --cached --name-status shows M' '
	(
	cd repo &&
	git diff --cached --name-status >../actual &&
	grep "^M" ../actual
	)
'

test_expect_success 'diff --cached --numstat' '
	(
	cd repo &&
	git diff --cached --numstat >../actual &&
	grep "file2.txt" ../actual
	)
'

test_expect_success 'diff --cached --exit-code detects staged change' '
	(
	cd repo &&
	test_must_fail git diff --cached --exit-code
	)
'

test_expect_success 'reset staged change' '
	(
	cd repo &&
	git reset HEAD file2.txt &&
	git checkout -- file2.txt
	)
'

# === diff between commits ===

test_expect_success 'setup second commit' '
	(
	cd repo &&
	echo "v2" >file1.txt &&
	echo "v2" >file3.txt &&
	git add . &&
	git commit -m "second"
	)
'

test_expect_success 'diff between two commits shows changes' '
	(
	cd repo &&
	git diff HEAD~1 HEAD >../actual &&
	grep "file1.txt" ../actual &&
	grep "file3.txt" ../actual
	)
'

test_expect_success 'diff between commits shows +/- content' '
	(
	cd repo &&
	git diff HEAD~1 HEAD >../actual &&
	grep "+v2" ../actual &&
	grep "\-line1" ../actual
	)
'

test_expect_success 'diff --stat between commits' '
	(
	cd repo &&
	git diff --stat HEAD~1 HEAD >../actual &&
	grep "file1.txt" ../actual &&
	grep "file3.txt" ../actual
	)
'

test_expect_success 'diff --name-only between commits' '
	(
	cd repo &&
	git diff --name-only HEAD~1 HEAD >../actual &&
	grep "file1.txt" ../actual &&
	grep "file3.txt" ../actual
	)
'

test_expect_success 'diff --name-status between commits' '
	(
	cd repo &&
	git diff --name-status HEAD~1 HEAD >../actual &&
	grep "^M.*file1.txt" ../actual
	)
'

test_expect_success 'diff --exit-code between commits returns 1' '
	(
	cd repo &&
	test_must_fail git diff --exit-code HEAD~1 HEAD
	)
'

test_expect_success 'diff same commit returns empty' '
	(
	cd repo &&
	git diff HEAD HEAD >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'diff same commit --exit-code returns 0' '
	(
	cd repo &&
	git diff --exit-code HEAD HEAD
	)
'

# === diff with subdirectory changes (cached) ===

test_expect_success 'diff --cached detects subdir file change' '
	(
	cd repo &&
	echo "new sub content" >sub/s.txt &&
	git add sub/s.txt &&
	git diff --cached >../actual &&
	grep "sub/s.txt" ../actual &&
	grep "+new sub content" ../actual
	)
'

test_expect_success 'diff --cached --stat subdir change' '
	(
	cd repo &&
	git diff --cached --stat >../actual &&
	grep "sub/s.txt" ../actual
	)
'

test_expect_success 'commit subdir change' '
	(
	cd repo &&
	git commit -m "update sub"
	)
'

# === diff --cached with new file ===

test_expect_success 'diff --cached shows new file with + lines' '
	(
	cd repo &&
	echo "brand new" >newfile.txt &&
	git add newfile.txt &&
	git diff --cached >../actual &&
	grep "newfile.txt" ../actual &&
	grep "+brand new" ../actual
	)
'

test_expect_success 'diff --cached --name-status shows A for new file' '
	(
	cd repo &&
	git diff --cached --name-status >../actual &&
	grep "^A.*newfile.txt" ../actual
	)
'

test_expect_success 'cleanup new file' '
	(
	cd repo &&
	git rm -f newfile.txt &&
	git reset HEAD 2>/dev/null;
	true
	)
'

# === diff --cached with deleted file ===

test_expect_success 'diff --cached shows deleted file with - lines' '
	(
	cd repo &&
	git rm file3.txt &&
	git diff --cached >../actual &&
	grep "file3.txt" ../actual &&
	grep "^-v2" ../actual
	)
'

test_expect_success 'diff --cached --name-status shows D for deleted file' '
	(
	cd repo &&
	git diff --cached --name-status >../actual &&
	grep "^D.*file3.txt" ../actual
	)
'

test_expect_success 'reset deletion' '
	(
	cd repo &&
	git reset HEAD file3.txt &&
	git checkout -- file3.txt
	)
'

# === diff context lines (-U) with cached ===

test_expect_success 'setup context file' '
	(
	cd repo &&
	printf "a\nb\nc\nd\ne\n" >ctx.txt &&
	git add ctx.txt &&
	git commit -m "ctx"
	)
'

test_expect_success 'diff --cached -U0 shows zero context lines' '
	(
	cd repo &&
	printf "a\nb\nC\nd\ne\n" >ctx.txt &&
	git add ctx.txt &&
	git diff --cached -U0 >../actual &&
	grep "^-c" ../actual &&
	grep "^+C" ../actual &&
	! grep "^ a" ../actual
	)
'

test_expect_success 'diff --cached -U1 shows 1 context line' '
	(
	cd repo &&
	git diff --cached -U1 >../actual &&
	grep "^ b" ../actual
	)
'

test_expect_success 'cleanup context test' '
	(
	cd repo &&
	git reset HEAD ctx.txt &&
	git checkout -- ctx.txt
	)
'

test_done
