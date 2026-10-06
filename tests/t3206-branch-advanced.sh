#!/bin/sh
test_description='grit branch advanced operations

Tests branch creation, deletion, renaming, copying, --show-current,
--contains, --merged, --no-merged, --force, and verbose listing.'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup: repo with linear history' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@test.com" &&
	echo "a" >file.txt && git add file.txt && git commit -m "first" &&
	echo "b" >file.txt && git add file.txt && git commit -m "second" &&
	echo "c" >file.txt && git add file.txt && git commit -m "third"
	)
'

# ── Basic listing ────────────────────────────────────────────────────────────

test_expect_success 'branch with no args lists branches' '
	(
	cd repo &&
	git branch >out &&
	grep "master" out
	)
'

test_expect_success 'branch --list works' '
	(
	cd repo &&
	git branch --list >out &&
	grep "master" out
	)
'

test_expect_success 'current branch is marked with asterisk' '
	(
	cd repo &&
	git branch >out &&
	grep "^\* master" out
	)
'

# ── --show-current ───────────────────────────────────────────────────────────

test_expect_success '--show-current shows current branch name' '
	(
	cd repo &&
	git branch --show-current >out &&
	grep "master" out
	)
'

test_expect_success '--show-current after checkout' '
	(
	cd repo &&
	git branch test-show &&
	git checkout test-show &&
	git branch --show-current >out &&
	grep "test-show" out &&
	git checkout master
	)
'

# ── Branch creation ──────────────────────────────────────────────────────────

test_expect_success 'create branch' '
	(
	cd repo &&
	git branch new-branch &&
	git branch >out &&
	grep "new-branch" out
	)
'

test_expect_success 'create branch at specific commit' '
	(
	cd repo &&
	first=$(git rev-parse HEAD~2) &&
	git branch old-branch "$first" &&
	git rev-parse old-branch >out &&
	echo "$first" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'create branch fails if name exists' '
	(
	cd repo &&
	test_must_fail git branch new-branch 2>err
	)
'

test_expect_success 'create branch with --force overwrites' '
	(
	cd repo &&
	prev=$(git rev-parse HEAD~1) &&
	git branch -f new-branch "$prev" &&
	git rev-parse new-branch >out &&
	echo "$prev" >expect &&
	test_cmp expect out
	)
'

# ── Branch deletion ──────────────────────────────────────────────────────────

test_expect_success 'delete branch with -d' '
	(
	cd repo &&
	git branch to-delete &&
	git branch -d to-delete &&
	git branch >out &&
	! grep "to-delete" out
	)
'

test_expect_success 'delete branch with --delete' '
	(
	cd repo &&
	git branch to-delete2 &&
	git branch --delete to-delete2 &&
	git branch >out &&
	! grep "to-delete2" out
	)
'

test_expect_success 'cannot delete current branch' '
	(
	cd repo &&
	test_must_fail git branch -d master 2>err
	)
'

test_expect_success 'force delete with -D' '
	(
	cd repo &&
	git branch force-del &&
	git branch -D force-del &&
	git branch >out &&
	! grep "force-del" out
	)
'

test_expect_success 'delete nonexistent branch fails' '
	(
	cd repo &&
	test_must_fail git branch -d no-such-branch 2>err
	)
'

# ── Branch renaming ─────────────────────────────────────────────────────────

test_expect_success 'rename branch with -m' '
	(
	cd repo &&
	git branch rename-me &&
	git branch -m rename-me renamed &&
	git branch >out &&
	grep "renamed" out &&
	! grep "rename-me" out
	)
'

test_expect_success 'force rename with -M' '
	(
	cd repo &&
	git branch target-name &&
	git branch -M renamed target-name &&
	git branch >out &&
	! grep "renamed" out
	)
'

# ── Branch copying ───────────────────────────────────────────────────────────

test_expect_success 'copy branch with -c' '
	(
	cd repo &&
	git branch src-branch &&
	git branch -c src-branch copied-branch &&
	git branch >out &&
	grep "src-branch" out &&
	grep "copied-branch" out
	)
'

test_expect_success 'branch at same commit verifiable via rev-parse' '
	(
	cd repo &&
	git branch verify-branch &&
	git rev-parse master >a &&
	git rev-parse verify-branch >b &&
	test_cmp a b
	)
'

# ── --contains ───────────────────────────────────────────────────────────────

test_expect_success '--contains HEAD shows current branch' '
	(
	cd repo &&
	git branch --contains HEAD >out &&
	grep "master" out
	)
'

test_expect_success '--contains old commit shows all branches' '
	(
	cd repo &&
	first=$(git rev-parse HEAD~2) &&
	git branch --contains "$first" >out &&
	grep "master" out
	)
'

# ── --merged / --no-merged ───────────────────────────────────────────────────

test_expect_success '--merged shows branches merged into HEAD' '
	(
	cd repo &&
	git branch --merged HEAD >out &&
	grep "master" out
	)
'

test_expect_success '--merged includes branch at same commit' '
	(
	cd repo &&
	git branch same-as-head &&
	git branch --merged HEAD >out &&
	grep "same-as-head" out
	)
'

test_expect_success 'setup: divergent branch for --no-merged' '
	(
	cd repo &&
	first=$(git rev-parse HEAD~2) &&
	git branch divergent "$first" &&
	git checkout divergent &&
	echo "diverge" >other.txt && git add other.txt && git commit -m "diverge" &&
	git checkout master
	)
'

test_expect_success '--no-merged shows unmerged branches' '
	(
	cd repo &&
	git branch --no-merged HEAD >out &&
	grep "divergent" out
	)
'

# ── Verbose listing ──────────────────────────────────────────────────────────

test_expect_success 'branch -v shows commit subjects' '
	(
	cd repo &&
	git branch -v >out &&
	grep "third" out
	)
'

test_expect_success 'branch -v shows short hash' '
	(
	cd repo &&
	git branch -v >out &&
	head=$(git rev-parse --short HEAD 2>/dev/null || git rev-parse HEAD | cut -c1-7) &&
	grep "$head" out
	)
'

# ── Quiet mode ───────────────────────────────────────────────────────────────

test_expect_success 'branch -q creates branch silently' '
	(
	cd repo &&
	git branch -q quiet-branch >out 2>&1 &&
	test_must_be_empty out &&
	git branch >list &&
	grep "quiet-branch" list
	)
'

test_expect_success 'branch -d -q deletes silently' '
	(
	cd repo &&
	git branch -d -q quiet-branch >out 2>&1 &&
	test_must_be_empty out
	)
'

test_done
