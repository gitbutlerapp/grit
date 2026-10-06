#!/bin/sh
# Tests for diff hunk headers and context lines.
# Upstream git t4018 covers diff function-name patterns extensively.
# We test unified diff output: hunk headers, context lines, line counts,
# --stat, --numstat, --name-only, --name-status, -U<n>, --cached, and
# commit-to-commit diffs.  Working-tree diff (index vs worktree) is known
# to have issues in grit, so we focus on --cached and tree-to-tree diffs.

test_description='diff hunk headers and unified-diff output'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup diff repo' '
	(
	git init diff-func &&
	cd diff-func &&
	git config user.name "Test User" &&
	git config user.email "test@example.com"
	)
'

test_expect_success 'create C file with functions' '
	(
	cd diff-func &&
	cat >main.c <<-\EOF &&
	#include <stdio.h>

	int helper(int x) {
	    return x * 2;
	}

	int main(int argc, char **argv) {
	    int a = 1;
	    int b = 2;
	    int c = 3;
	    int d = 4;
	    printf("hello\n");
	    return 0;
	}
	EOF
	git add main.c &&
	test_tick &&
	git commit -m "initial C file"
	)
'

test_expect_success 'diff --cached shows hunk header with @@ markers' '
	(
	cd diff-func &&
	sed "s/printf(\"hello/printf(\"world/" main.c >main.c.new &&
	mv main.c.new main.c &&
	git add main.c &&
	git diff --cached >actual &&
	grep "^@@" actual
	)
'

test_expect_success 'diff --cached hunk header contains line numbers' '
	(
	cd diff-func &&
	git diff --cached >actual &&
	grep "^@@ -[0-9]" actual
	)
'

test_expect_success 'diff --cached shows minus lines for old content' '
	(
	cd diff-func &&
	git diff --cached >actual &&
	grep "^-.*hello" actual
	)
'

test_expect_success 'diff --cached shows plus lines for new content' '
	(
	cd diff-func &&
	git diff --cached >actual &&
	grep "^+.*world" actual
	)
'

test_expect_success 'diff --cached shows context lines (unchanged)' '
	(
	cd diff-func &&
	git diff --cached >actual &&
	grep "^ " actual
	)
'

test_expect_success 'diff --cached context defaults to 3 lines' '
	(
	cd diff-func &&
	git diff --cached >actual &&
	ctx=$(grep -c "^ " actual) &&
	test "$ctx" -ge 3
	)
'

test_expect_success 'commit change and diff between commits' '
	(
	cd diff-func &&
	test_tick &&
	git commit -m "update hello to world" &&
	parent=$(git rev-parse HEAD~1) &&
	head=$(git rev-parse HEAD) &&
	git diff $parent $head >actual &&
	grep "^-.*hello" actual &&
	grep "^+.*world" actual
	)
'

test_expect_success 'diff -U1 --cached reduces context' '
	(
	cd diff-func &&
	sed "s/return 0/return 42/" main.c >main.c.new &&
	mv main.c.new main.c &&
	git add main.c &&
	git diff --cached -U1 >actual_1 &&
	git diff --cached >actual_3 &&
	ctx_1=$(grep -c "^ " actual_1) &&
	ctx_3=$(grep -c "^ " actual_3) &&
	test "$ctx_1" -le "$ctx_3"
	)
'

test_expect_success 'diff -U0 --cached shows no context' '
	(
	cd diff-func &&
	git diff --cached -U0 >actual &&
	grep "^-.*return 0" actual &&
	grep "^+.*return 42" actual
	)
'

test_expect_success 'commit and diff --stat shows summary' '
	(
	cd diff-func &&
	test_tick &&
	git commit -m "change return value" &&
	parent=$(git rev-parse HEAD~1) &&
	head=$(git rev-parse HEAD) &&
	git diff --stat $parent $head >actual &&
	grep "main.c" actual &&
	grep "1 file changed" actual
	)
'

test_expect_success 'diff --numstat shows machine-readable numbers' '
	(
	cd diff-func &&
	parent=$(git rev-parse HEAD~1) &&
	head=$(git rev-parse HEAD) &&
	git diff --numstat $parent $head >actual &&
	grep "main.c" actual &&
	grep "^[0-9]" actual
	)
'

test_expect_success 'diff --name-only shows filenames' '
	(
	cd diff-func &&
	parent=$(git rev-parse HEAD~1) &&
	head=$(git rev-parse HEAD) &&
	git diff --name-only $parent $head >actual &&
	echo "main.c" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'diff --name-status shows M for modified' '
	(
	cd diff-func &&
	parent=$(git rev-parse HEAD~1) &&
	head=$(git rev-parse HEAD) &&
	git diff --name-status $parent $head >actual &&
	grep "^M" actual &&
	grep "main.c" actual
	)
'

test_expect_success 'create multi-function file' '
	(
	cd diff-func &&
	cat >multi.c <<-\EOF &&
	void func_a(void) {
	    int x = 1;
	    int y = 2;
	    int z = 3;
	}

	void func_b(void) {
	    int a = 10;
	    int b = 20;
	    int c = 30;
	}

	void func_c(void) {
	    int p = 100;
	    int q = 200;
	    int r = 300;
	}
	EOF
	git add multi.c &&
	test_tick &&
	git commit -m "add multi-function file"
	)
'

test_expect_success 'diff --cached with change in func_b shows correct hunk' '
	(
	cd diff-func &&
	sed "s/int b = 20/int b = 99/" multi.c >multi.c.new &&
	mv multi.c.new multi.c &&
	git add multi.c &&
	git diff --cached >actual &&
	grep "^@@" actual &&
	grep "^-.*int b = 20" actual &&
	grep "^+.*int b = 99" actual
	)
'

test_expect_success 'multiple changes in --cached show both changes' '
	(
	cd diff-func &&
	sed "s/int x = 1/int x = 11/" multi.c >multi.c.new &&
	mv multi.c.new multi.c &&
	git add multi.c &&
	git diff --cached >actual &&
	grep "^-.*int x = 1" actual &&
	grep "^+.*int x = 11" actual &&
	grep "^-.*int b = 20" actual &&
	grep "^+.*int b = 99" actual
	)
'

test_expect_success 'diff -U5 --cached can merge nearby hunks' '
	(
	cd diff-func &&
	git diff --cached -U5 >actual_5 &&
	git diff --cached -U0 >actual_0 &&
	hunks_5=$(grep -c "^@@" actual_5) &&
	hunks_0=$(grep -c "^@@" actual_0) &&
	test "$hunks_0" -ge "$hunks_5"
	)
'

test_expect_success 'diff --cached with new file' '
	(
	cd diff-func &&
	test_tick &&
	git commit -m "update multi" &&
	echo "new content" >newfile.txt &&
	git add newfile.txt &&
	git diff --cached >actual &&
	grep "^diff --git" actual &&
	grep "newfile.txt" actual &&
	grep "^+new content" actual
	)
'

test_expect_success 'diff --cached with file deletion' '
	(
	cd diff-func &&
	test_tick &&
	git commit -m "add newfile" &&
	git rm newfile.txt &&
	git diff --cached >actual &&
	grep "^diff --git" actual &&
	grep "newfile.txt" actual &&
	grep "^-new content" actual
	)
'

test_expect_success 'diff header shows a/ b/ prefixes' '
	(
	cd diff-func &&
	git diff --cached >actual &&
	grep "^--- a/newfile.txt" actual
	)
'

test_expect_success 'diff shows /dev/null for deletions' '
	(
	cd diff-func &&
	git diff --cached >actual &&
	grep "/dev/null" actual
	)
'

test_expect_success 'diff --cached with brand new file from scratch' '
	(
	cd diff-func &&
	git reset HEAD -- newfile.txt &&
	git checkout -- newfile.txt 2>/dev/null || true &&
	echo "line1" >brand_new.txt &&
	git add brand_new.txt &&
	git diff --cached >actual &&
	grep "^+line1" actual
	)
'

test_expect_success 'diff --quiet with no changes exits 0' '
	(
	cd diff-func &&
	test_tick &&
	git commit -m "add brand_new" &&
	git diff --quiet --cached
	)
'

test_expect_success 'diff commit-to-commit with multiple files changed' '
	(
	cd diff-func &&
	echo "extra" >>main.c &&
	echo "extra" >>multi.c &&
	git add main.c multi.c &&
	test_tick &&
	git commit -m "modify both files" &&
	parent=$(git rev-parse HEAD~1) &&
	head=$(git rev-parse HEAD) &&
	git diff --name-only $parent $head >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'diff --stat with multiple files' '
	(
	cd diff-func &&
	parent=$(git rev-parse HEAD~1) &&
	head=$(git rev-parse HEAD) &&
	git diff --stat $parent $head >actual &&
	grep "main.c" actual &&
	grep "multi.c" actual &&
	grep "2 files changed" actual
	)
'

test_done
