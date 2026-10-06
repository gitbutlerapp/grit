#!/bin/sh
# Tests for grit switch with branch creation, detaching, orphan branches,
# and various switching scenarios. grit switch forwards to system git.

test_description='grit switch --create --detach and related options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with commits on main' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@example.com" &&
	git config user.name "Test User" &&
	echo "file1" >file1.txt &&
	grit add file1.txt &&
	grit commit -m "first commit" &&
	echo "file2" >file2.txt &&
	grit add file2.txt &&
	grit commit -m "second commit" &&
	echo "file3" >file3.txt &&
	grit add file3.txt &&
	grit commit -m "third commit"
	)
'

# --- basic switch ---

test_expect_success 'switch -c creates and switches to new branch' '
	(
	cd repo &&
	grit switch -c feature1 &&
	grit branch --show-current >out &&
	grep "feature1" out
	)
'

test_expect_success 'switch back to main' '
	(
	cd repo &&
	grit switch main &&
	grit branch --show-current >out &&
	grep "main" out
	)
'

test_expect_success 'switch -c from specific start point' '
	(
	cd repo &&
	FIRST=$(grit rev-parse HEAD~2) &&
	grit switch -c from-first "$FIRST" &&
	grit rev-parse HEAD >head_out &&
	echo "$FIRST" >expect &&
	diff expect head_out
	)
'

test_expect_success 'switch -c to existing branch fails' '
	(
	cd repo &&
	grit switch main &&
	grit switch -c temp-branch &&
	grit switch main &&
	test_must_fail grit switch -c temp-branch 2>err
	)
'

# --- detach ---

test_expect_success 'switch --detach goes to detached HEAD' '
	(
	cd repo &&
	grit switch main &&
	HEAD_SHA=$(grit rev-parse HEAD) &&
	grit switch --detach HEAD &&
	grit rev-parse HEAD >head_out &&
	echo "$HEAD_SHA" >expect &&
	diff expect head_out
	)
'

test_expect_success 'switch --detach to specific commit' '
	(
	cd repo &&
	FIRST=$(grit rev-parse main~2) &&
	grit switch --detach "$FIRST" &&
	grit rev-parse HEAD >head_out &&
	echo "$FIRST" >expect &&
	diff expect head_out
	)
'

test_expect_success 'switch -d is alias for --detach' '
	(
	cd repo &&
	grit switch main &&
	MAIN_HEAD=$(grit rev-parse HEAD) &&
	grit switch -d HEAD &&
	grit rev-parse HEAD >head_out &&
	echo "$MAIN_HEAD" >expect &&
	diff expect head_out
	)
'

test_expect_success 'switch from detached HEAD to branch' '
	(
	cd repo &&
	grit switch --detach HEAD~1 &&
	grit switch main &&
	grit branch --show-current >out &&
	grep "main" out
	)
'

# --- orphan ---

test_expect_success 'switch --orphan creates orphan branch' '
	(
	cd repo &&
	grit switch --orphan orphan-branch &&
	grit branch --show-current >out &&
	grep "orphan-branch" out
	)
'

test_expect_success 'orphan branch has no commits yet' '
	(
	cd repo &&
	grit switch --orphan empty-branch &&
	test_must_fail grit rev-parse HEAD 2>err
	)
'

test_expect_success 'switch --orphan clears index' '
	(
	cd repo &&
	grit switch main &&
	grit switch --orphan clean-orphan &&
	grit ls-files >ls_out &&
	test_must_be_empty ls_out
	)
'

# --- switching with uncommitted changes ---

test_expect_success 'switch fails with uncommitted changes that conflict' '
	(
	cd repo &&
	grit switch main &&
	grit switch -c diverge1 &&
	echo "diverge" >file1.txt &&
	grit add file1.txt &&
	grit commit -m "diverge file1" &&
	grit switch main &&
	echo "conflict" >file1.txt &&
	test_must_fail grit switch diverge1 2>err
	)
'

test_expect_success 'switch succeeds with uncommitted changes to unrelated files' '
	(
	cd repo &&
	grit switch main &&
	grit checkout -- file1.txt &&
	echo "unrelated" >unrelated_new.txt &&
	grit switch -c safe-switch &&
	test -f unrelated_new.txt &&
	grit switch main &&
	rm -f unrelated_new.txt
	)
'

# --- branch listing / verification ---

test_expect_success 'switch -c creates branch visible in branch list' '
	(
	cd repo &&
	grit switch main &&
	grit switch -c visible-branch &&
	grit branch >branches &&
	grep "visible-branch" branches
	)
'

test_expect_success 'switch to nonexistent branch fails' '
	(
	cd repo &&
	test_must_fail grit switch nonexistent-branch 2>err
	)
'

test_expect_success 'switch -c with already existing branch name fails' '
	(
	cd repo &&
	grit switch main &&
	grit switch -c dup-test &&
	grit switch main &&
	test_must_fail grit switch -c dup-test 2>err
	)
'

# --- multiple branches ---

test_expect_success 'create and switch between multiple branches' '
	(
	cd repo &&
	grit switch main &&
	grit switch -c branch-a &&
	echo "a-content" >a.txt &&
	grit add a.txt &&
	grit commit -m "branch-a commit" &&
	grit switch main &&
	! test -f a.txt &&
	grit switch -c branch-b &&
	echo "b-content" >b.txt &&
	grit add b.txt &&
	grit commit -m "branch-b commit" &&
	grit switch branch-a &&
	test -f a.txt &&
	! test -f b.txt &&
	grit switch branch-b &&
	test -f b.txt &&
	! test -f a.txt
	)
'

test_expect_success 'switch preserves working tree for current branch' '
	(
	cd repo &&
	grit switch main &&
	MAIN_FILES=$(grit ls-files | sort) &&
	grit switch branch-a &&
	grit switch main &&
	MAIN_FILES2=$(grit ls-files | sort) &&
	test "$MAIN_FILES" = "$MAIN_FILES2"
	)
'

# --- switch with -- separator ---

test_expect_success 'switch -- disambiguates branch from path' '
	(
	cd repo &&
	grit switch main &&
	grit switch -c file1.txt-branch &&
	grit switch main &&
	grit switch -- file1.txt-branch &&
	grit branch --show-current >out &&
	grep "file1.txt-branch" out
	)
'

# --- detach at tag ---

test_expect_success 'setup tags for detach tests' '
	(
	cd repo &&
	grit switch main &&
	grit tag v1.0 HEAD~2 &&
	grit tag v2.0 HEAD~1 &&
	grit tag v3.0 HEAD
	)
'

test_expect_success 'switch --detach at tag' '
	(
	cd repo &&
	grit switch --detach v1.0 &&
	EXPECTED=$(grit rev-parse v1.0) &&
	ACTUAL=$(grit rev-parse HEAD) &&
	test "$EXPECTED" = "$ACTUAL"
	)
'

test_expect_success 'switch --detach at different tag' '
	(
	cd repo &&
	grit switch --detach v2.0 &&
	EXPECTED=$(grit rev-parse v2.0) &&
	ACTUAL=$(grit rev-parse HEAD) &&
	test "$EXPECTED" = "$ACTUAL"
	)
'

# --- switch -c from tag ---

test_expect_success 'switch -c from tag start point' '
	(
	cd repo &&
	grit switch -c from-tag v1.0 &&
	grit branch --show-current >out &&
	grep "from-tag" out &&
	EXPECTED=$(grit rev-parse v1.0) &&
	ACTUAL=$(grit rev-parse HEAD) &&
	test "$EXPECTED" = "$ACTUAL"
	)
'

# --- switch guess ---

test_expect_success 'switch back to main after all tests' '
	(
	cd repo &&
	grit switch main &&
	grit branch --show-current >out &&
	grep "main" out
	)
'

test_expect_success 'switch --detach HEAD is same as current' '
	(
	cd repo &&
	BEFORE=$(grit rev-parse HEAD) &&
	grit switch --detach HEAD &&
	AFTER=$(grit rev-parse HEAD) &&
	test "$BEFORE" = "$AFTER"
	)
'

test_expect_success 'switch -c with spaces in name fails' '
	(
	cd repo &&
	test_must_fail grit switch -c "bad branch name" 2>err
	)
'

test_expect_success 'switch -c from HEAD~N' '
	(
	cd repo &&
	grit switch main &&
	grit switch -c from-head-1 HEAD~1 &&
	EXPECTED=$(grit rev-parse main~1) &&
	ACTUAL=$(grit rev-parse HEAD) &&
	test "$EXPECTED" = "$ACTUAL"
	)
'

test_done
