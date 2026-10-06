#!/bin/sh
# Tests for checkout with conflicting changes, error messages

test_description='checkout with conflicting changes'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup repository with two branches' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "base content" >file.txt &&
	echo "other" >safe.txt &&
	git add file.txt safe.txt &&
	git commit -m "initial" &&
	git checkout -b branch-a &&
	echo "branch-a content" >file.txt &&
	git add file.txt &&
	git commit -m "branch-a change" &&
	git checkout master &&
	git checkout -b branch-b &&
	echo "branch-b content" >file.txt &&
	git add file.txt &&
	git commit -m "branch-b change"
	)
'

# ---------------------------------------------------------------------------
# Dirty working tree prevents checkout
# ---------------------------------------------------------------------------
test_expect_success 'checkout fails with dirty tracked file that conflicts' '
	(
	cd repo &&
	git checkout branch-a &&
	echo "dirty modification" >file.txt &&
	test_must_fail git checkout branch-b 2>err
	)
'

test_expect_success 'error mentions the conflicting file' '
	(
	cd repo &&
	grep -q "file.txt" err || grep -qi "overwritten\|conflict\|local changes\|not clean" err
	)
'

test_expect_success 'working tree is unchanged after failed checkout' '
	(
	cd repo &&
	echo "dirty modification" >expected &&
	test_cmp expected file.txt
	)
'

test_expect_success 'HEAD still points to original branch after failed checkout' '
	(
	cd repo &&
	test "$(git symbolic-ref --short HEAD)" = "branch-a"
	)
'

# ---------------------------------------------------------------------------
# Clean working tree allows checkout
# ---------------------------------------------------------------------------
test_expect_success 'checkout succeeds after resetting dirty file' '
	(
	cd repo &&
	git checkout -- file.txt &&
	git checkout branch-b
	)
'

test_expect_success 'file has branch-b content' '
	(
	cd repo &&
	echo "branch-b content" >expected &&
	test_cmp expected file.txt
	)
'

# ---------------------------------------------------------------------------
# Untracked file conflict
# ---------------------------------------------------------------------------
test_expect_success 'setup: branch with new file' '
	(
	cd repo &&
	git checkout master &&
	git checkout -b branch-newfile &&
	echo "tracked content" >newfile.txt &&
	git add newfile.txt &&
	git commit -m "add newfile"
	)
'

test_expect_success 'checkout fails when untracked file would be overwritten' '
	(
	cd repo &&
	git checkout master &&
	echo "untracked content" >newfile.txt &&
	test_must_fail git checkout branch-newfile 2>err
	)
'

test_expect_success 'untracked file is preserved after failed checkout' '
	(
	cd repo &&
	echo "untracked content" >expected &&
	test_cmp expected newfile.txt
	)
'

test_expect_success 'checkout succeeds after removing conflicting untracked file' '
	(
	cd repo &&
	rm newfile.txt &&
	git checkout branch-newfile &&
	echo "tracked content" >expected &&
	test_cmp expected newfile.txt
	)
'

# ---------------------------------------------------------------------------
# Staged changes conflict
# ---------------------------------------------------------------------------
test_expect_success 'setup: stage a conflicting change' '
	(
	cd repo &&
	git checkout branch-a &&
	echo "staged change" >file.txt &&
	git add file.txt
	)
'

test_expect_success 'checkout fails with staged changes that conflict' '
	(
	cd repo &&
	test_must_fail git checkout branch-b 2>err
	)
'

test_expect_success 'staged content is preserved after failed checkout' '
	(
	cd repo &&
	git diff --cached --name-only >out &&
	grep -q "file.txt" out
	)
'

test_expect_success 'reset staged changes allows checkout' '
	(
	cd repo &&
	git reset HEAD -- file.txt &&
	git checkout -- file.txt &&
	git checkout branch-b
	)
'

# ---------------------------------------------------------------------------
# Checkout file from another branch
# ---------------------------------------------------------------------------
test_expect_success 'checkout specific file from another branch' '
	(
	cd repo &&
	git checkout branch-a -- file.txt &&
	echo "branch-a content" >expected &&
	test_cmp expected file.txt
	)
'

test_expect_success 'file from other branch is staged' '
	(
	cd repo &&
	git diff --cached --name-only >out &&
	grep -q "file.txt" out
	)
'

test_expect_success 'HEAD still on branch-b after file checkout' '
	(
	cd repo &&
	test "$(git symbolic-ref --short HEAD)" = "branch-b"
	)
'

# ---------------------------------------------------------------------------
# Checkout with force
# ---------------------------------------------------------------------------
test_expect_success 'setup dirty file for force checkout' '
	(
	cd repo &&
	git checkout -- file.txt &&
	git checkout branch-a &&
	echo "will be overwritten" >file.txt
	)
'

test_expect_success 'checkout -f discards dirty changes' '
	(
	cd repo &&
	git checkout -f branch-b &&
	echo "branch-b content" >expected &&
	test_cmp expected file.txt
	)
'

test_expect_success 'HEAD is on branch-b after force checkout' '
	(
	cd repo &&
	test "$(git symbolic-ref --short HEAD)" = "branch-b"
	)
'

# ---------------------------------------------------------------------------
# Checkout -- file restores working tree
# ---------------------------------------------------------------------------
test_expect_success 'checkout -- file restores to index version' '
	(
	cd repo &&
	echo "modified" >safe.txt &&
	git checkout -- safe.txt &&
	echo "other" >expected &&
	test_cmp expected safe.txt
	)
'

test_expect_success 'checkout -- . restores all modified files' '
	(
	cd repo &&
	echo "mod1" >file.txt &&
	echo "mod2" >safe.txt &&
	git checkout -- . &&
	echo "branch-b content" >expected_file &&
	echo "other" >expected_safe &&
	test_cmp expected_file file.txt &&
	test_cmp expected_safe safe.txt
	)
'

# ---------------------------------------------------------------------------
# Non-conflicting dirty changes are preserved
# ---------------------------------------------------------------------------
test_expect_success 'setup: create branches with non-overlapping changes' '
	(
	cd repo &&
	git checkout master &&
	git checkout -b left &&
	echo "left" >left.txt &&
	git add left.txt &&
	git commit -m "add left" &&
	git checkout master &&
	git checkout -b right &&
	echo "right" >right.txt &&
	git add right.txt &&
	git commit -m "add right"
	)
'

test_expect_success 'dirty non-conflicting file survives checkout' '
	(
	cd repo &&
	echo "dirty safe" >safe.txt &&
	git checkout left &&
	echo "dirty safe" >expected &&
	test_cmp expected safe.txt
	)
'

test_expect_success 'clean up dirty file' '
	(
	cd repo &&
	git checkout -- safe.txt
	)
'

# ---------------------------------------------------------------------------
# Checkout detached HEAD with dirty worktree
# ---------------------------------------------------------------------------
test_expect_success 'detached HEAD checkout fails with conflicting dirty file' '
	(
	cd repo &&
	git checkout branch-a &&
	echo "dirty" >file.txt &&
	sha=$(git rev-parse branch-b) &&
	test_must_fail git checkout "$sha" 2>err
	)
'

test_expect_success 'still on branch-a after failed detached checkout' '
	(
	cd repo &&
	test "$(git symbolic-ref --short HEAD)" = "branch-a"
	)
'

test_expect_success 'force detached checkout works' '
	(
	cd repo &&
	sha=$(git rev-parse branch-b) &&
	git checkout -f "$sha" 2>/dev/null &&
	head_sha=$(git rev-parse HEAD) &&
	test "$head_sha" = "$sha"
	)
'

test_done
