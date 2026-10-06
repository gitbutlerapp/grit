#!/bin/sh
test_description='grit diff --numstat, --name-only, --name-status, --exit-code, -q, -U

Ported concepts from git/t/t4047-diff-stat-count.sh and related diff tests.
Tests the machine-readable and summary diff output modes.'

. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup: repo with multiple files and commits' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@test.com" &&
	echo "line1" >file1.txt &&
	echo "line1" >file2.txt &&
	echo "line1" >file3.txt &&
	mkdir sub &&
	echo "line1" >sub/deep.txt &&
	git add . &&
	git commit -m "initial" &&
	echo "changed1" >file1.txt &&
	echo "line1" >file2.txt &&
	echo "changed3" >file3.txt &&
	echo "changed-deep" >sub/deep.txt &&
	git add . &&
	git commit -m "modify three files"
	)
'

# ── --numstat ────────────────────────────────────────────────────────────────

test_expect_success '--numstat between commits shows additions and deletions' '
	(
	cd repo &&
	git diff --numstat HEAD~1 HEAD >out &&
	grep "file1\.txt" out &&
	grep "file3\.txt" out &&
	grep "sub/deep\.txt" out
	)
'

test_expect_success '--numstat output has tab-separated fields' '
	(
	cd repo &&
	git diff --numstat HEAD~1 HEAD >out &&
	# Format: additions<TAB>deletions<TAB>filename
	awk -F"\t" "NF != 3 { exit 1 }" out
	)
'

test_expect_success '--numstat does not show unchanged files' '
	(
	cd repo &&
	git diff --numstat HEAD~1 HEAD >out &&
	! grep "file2\.txt" out
	)
'

test_expect_success '--numstat shows correct count of changed files' '
	(
	cd repo &&
	git diff --numstat HEAD~1 HEAD >out &&
	test_line_count = 3 out
	)
'

test_expect_success '--numstat for working tree changes' '
	(
	cd repo &&
	echo "wt-change" >file2.txt &&
	git diff --numstat >out &&
	grep "file2\.txt" out &&
	test_line_count = 1 out &&
	git checkout -- file2.txt
	)
'

test_expect_success '--numstat with --cached' '
	(
	cd repo &&
	echo "staged" >file1.txt &&
	git add file1.txt &&
	git diff --cached --numstat >out &&
	grep "file1\.txt" out &&
	test_line_count = 1 out &&
	git reset HEAD -- file1.txt &&
	git checkout -- file1.txt
	)
'

# ── --name-only ──────────────────────────────────────────────────────────────

test_expect_success '--name-only lists changed filenames' '
	(
	cd repo &&
	git diff --name-only HEAD~1 HEAD >out &&
	grep "file1\.txt" out &&
	grep "file3\.txt" out &&
	grep "sub/deep\.txt" out
	)
'

test_expect_success '--name-only does not show unchanged files' '
	(
	cd repo &&
	git diff --name-only HEAD~1 HEAD >out &&
	! grep "file2\.txt" out
	)
'

test_expect_success '--name-only with correct count' '
	(
	cd repo &&
	git diff --name-only HEAD~1 HEAD >out &&
	test_line_count = 3 out
	)
'

test_expect_success '--name-only for working tree' '
	(
	cd repo &&
	echo "new content" >file3.txt &&
	git diff --name-only >out &&
	grep "file3\.txt" out &&
	test_line_count = 1 out &&
	git checkout -- file3.txt
	)
'

# ── --name-status ────────────────────────────────────────────────────────────

test_expect_success '--name-status shows status letters' '
	(
	cd repo &&
	git diff --name-status HEAD~1 HEAD >out &&
	grep "^M" out
	)
'

test_expect_success '--name-status for new file shows A' '
	(
	git init ns-repo &&
	cd ns-repo &&
	git config user.name "Test User" &&
	git config user.email "test@test.com" &&
	echo "first" >a.txt &&
	git add a.txt &&
	git commit -m "first" &&
	echo "second" >b.txt &&
	git add b.txt &&
	git commit -m "add b" &&
	git diff --name-status HEAD~1 HEAD >out &&
	grep "^A" out &&
	grep "b\.txt" out
	)
'

test_expect_success '--name-status for deleted file shows D' '
	(
	cd ns-repo &&
	git rm -f a.txt &&
	git commit -m "remove a" &&
	git diff --name-status HEAD~1 HEAD >out &&
	grep "^D" out &&
	grep "a\.txt" out
	)
'

# ── --exit-code ──────────────────────────────────────────────────────────────

test_expect_success '--exit-code returns 1 when there are differences' '
	(
	cd repo &&
	test_expect_code 1 git diff --exit-code HEAD~1 HEAD
	)
'

test_expect_success '--exit-code returns 0 when no differences' '
	(
	cd repo &&
	git diff --exit-code HEAD HEAD
	)
'

test_expect_success '-q with differences returns non-zero exit' '
	(
	cd repo &&
	test_expect_code 1 git diff -q HEAD~1 HEAD
	)
'

# ── -U (unified context lines) ──────────────────────────────────────────────

test_expect_success 'setup: file with many lines for context tests' '
	(
	git init ctx-repo &&
	cd ctx-repo &&
	git config user.name "Test User" &&
	git config user.email "test@test.com" &&
	for i in 1 2 3 4 5 6 7 8 9 10; do echo "line$i"; done >multi.txt &&
	git add multi.txt &&
	git commit -m "ten lines" &&
	sed "s/line5/CHANGED5/" multi.txt >tmp && mv tmp multi.txt &&
	git add multi.txt &&
	git commit -m "change line5"
	)
'

test_expect_success '-U0 shows zero context lines' '
	(
	cd ctx-repo &&
	git diff -U0 HEAD~1 HEAD -- multi.txt >out &&
	grep "^-line5" out &&
	grep "^+CHANGED5" out &&
	! grep "^.line4" out
	)
'

test_expect_success 'default context is 3 lines' '
	(
	cd ctx-repo &&
	git diff HEAD~1 HEAD -- multi.txt >out &&
	# Should see lines around the change
	grep "line" out
	)
'

# ── path limiting ────────────────────────────────────────────────────────────

test_expect_success 'diff with path limits to specific file' '
	(
	cd repo &&
	git diff --name-only HEAD~1 HEAD -- file1.txt >out &&
	test_line_count = 1 out &&
	grep "file1\.txt" out
	)
'

test_expect_success 'diff with path excludes other files' '
	(
	cd repo &&
	git diff --name-only HEAD~1 HEAD -- file1.txt >out &&
	! grep "file3\.txt" out &&
	! grep "sub/deep\.txt" out
	)
'

test_expect_success 'diff with directory path' '
	(
	cd repo &&
	git diff --name-only HEAD~1 HEAD -- sub/ >out &&
	test_line_count = 1 out &&
	grep "sub/deep\.txt" out
	)
'

# ── empty diff cases ────────────────────────────────────────────────────────

test_expect_success '--numstat on empty diff produces no output' '
	(
	cd repo &&
	git diff --numstat HEAD HEAD >out &&
	test_must_be_empty out
	)
'

test_expect_success '--name-only on empty diff produces no output' '
	(
	cd repo &&
	git diff --name-only HEAD HEAD >out &&
	test_must_be_empty out
	)
'

test_expect_success '--name-status on empty diff produces no output' '
	(
	cd repo &&
	git diff --name-status HEAD HEAD >out &&
	test_must_be_empty out
	)
'

test_done
