#!/bin/sh
# Tests for branch: listing, creation, deletion, rename, -v, --show-current.

test_description='branch listing, creation, deletion, and verbose info'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with branches' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "base" >file.txt &&
	git add file.txt &&
	git commit -m "initial commit" &&
	git branch feature-a &&
	git branch feature-b &&
	echo "update" >file.txt &&
	git add file.txt &&
	git commit -m "second commit" &&
	git branch feature-c &&
	echo "third" >file.txt &&
	git add file.txt &&
	git commit -m "third commit"
	)
'

# ── Basic listing ────────────────────────────────────────────────────────

test_expect_success 'branch: lists all local branches' '
	(
	cd repo &&
	git branch >out &&
	grep "master" out &&
	grep "feature-a" out &&
	grep "feature-b" out &&
	grep "feature-c" out
	)
'

test_expect_success 'branch: current branch has asterisk' '
	(
	cd repo &&
	git branch >out &&
	grep "^\* master" out
	)
'

test_expect_success 'branch --show-current: shows current branch name' '
	(
	cd repo &&
	git branch --show-current >out &&
	echo "master" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'branch: total count is correct' '
	(
	cd repo &&
	git branch >out &&
	test_line_count = 4 out
	)
'

# ── -v (verbose) ─────────────────────────────────────────────────────────

test_expect_success 'branch -v: shows commit hash prefix' '
	(
	cd repo &&
	git branch -v >out &&
	grep -E "[0-9a-f]{7}" out
	)
'

test_expect_success 'branch -v: shows commit subject for master' '
	(
	cd repo &&
	git branch -v >out &&
	grep "master" out | grep "third commit"
	)
'

test_expect_success 'branch -v: feature-a shows initial commit subject' '
	(
	cd repo &&
	git branch -v >out &&
	grep "feature-a" out | grep "initial commit"
	)
'

test_expect_success 'branch -v: feature-c shows second commit subject' '
	(
	cd repo &&
	git branch -v >out &&
	grep "feature-c" out | grep "second commit"
	)
'

test_expect_success 'branch -vv: similar to -v output' '
	(
	cd repo &&
	git branch -vv >out &&
	grep "master" out &&
	grep -E "[0-9a-f]{7}" out
	)
'

# ── Branch creation ──────────────────────────────────────────────────────

test_expect_success 'branch: create new branch at HEAD' '
	(
	cd repo &&
	git branch new-branch &&
	git branch >out &&
	grep "new-branch" out
	)
'

test_expect_success 'branch: new branch points to HEAD' '
	(
	cd repo &&
	new_oid=$(git rev-parse new-branch) &&
	head_oid=$(git rev-parse HEAD) &&
	test "$new_oid" = "$head_oid"
	)
'

test_expect_success 'branch: create from specific SHA' '
	(
	cd repo &&
	first_oid=$(git rev-parse master~2) &&
	git branch from-first "$first_oid" &&
	actual=$(git rev-parse from-first) &&
	test "$first_oid" = "$actual"
	)
'

test_expect_success 'branch: create from another branch name' '
	(
	cd repo &&
	git branch from-feature feature-a &&
	oid_a=$(git rev-parse feature-a) &&
	oid_from=$(git rev-parse from-feature) &&
	test "$oid_a" = "$oid_from"
	)
'

test_expect_success 'branch: duplicate name fails without -f' '
	(
	cd repo &&
	test_must_fail git branch feature-a
	)
'

test_expect_success 'branch -f: force overwrite existing branch' '
	(
	cd repo &&
	old_oid=$(git rev-parse feature-a) &&
	git branch -f feature-a HEAD &&
	new_oid=$(git rev-parse feature-a) &&
	test "$old_oid" != "$new_oid"
	)
'

# ── Branch deletion ──────────────────────────────────────────────────────

test_expect_success 'branch -d: delete branch' '
	(
	cd repo &&
	git branch -d new-branch &&
	git branch >out &&
	! grep "new-branch" out
	)
'

test_expect_success 'branch -d: cannot delete current branch' '
	(
	cd repo &&
	test_must_fail git branch -d master
	)
'

test_expect_success 'branch -D: force delete branch' '
	(
	cd repo &&
	git branch force-del &&
	git branch -D force-del &&
	git branch >out &&
	! grep "force-del" out
	)
'

test_expect_success 'branch -d --quiet: no output on delete' '
	(
	cd repo &&
	git branch quiet-del &&
	git branch -d --quiet quiet-del >out 2>&1 &&
	test_must_be_empty out
	)
'

# ── Branch rename ────────────────────────────────────────────────────────

test_expect_success 'branch -m: rename branch' '
	(
	cd repo &&
	git branch rename-me &&
	git branch -m rename-me renamed &&
	git branch >out &&
	grep "renamed" out &&
	! grep "rename-me" out
	)
'

test_expect_success 'branch -m: renamed branch has same commit' '
	(
	cd repo &&
	oid=$(git rev-parse renamed) &&
	master_oid=$(git rev-parse master) &&
	test "$oid" = "$master_oid"
	)
'

test_expect_success 'branch -M: force rename' '
	(
	cd repo &&
	git branch force-rename-src &&
	git branch -M force-rename-src force-rename-dst &&
	git branch >out &&
	grep "force-rename-dst" out &&
	! grep "force-rename-src" out
	)
'

# ── --all / --remotes ───────────────────────────────────────────────────

test_expect_success 'branch -a: includes all local branches' '
	(
	cd repo &&
	git branch -a >out &&
	grep "master" out &&
	grep "feature-a" out
	)
'

test_expect_success 'branch -r: empty when no remotes configured' '
	(
	cd repo &&
	git branch -r >out &&
	test_must_be_empty out
	)
'

# ── Checkout integration ─────────────────────────────────────────────────

test_expect_success 'checkout changes current branch' '
	(
	cd repo &&
	git checkout feature-c &&
	git branch --show-current >out &&
	echo "feature-c" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'branch: asterisk moves to checked out branch' '
	(
	cd repo &&
	git checkout feature-c &&
	git branch >out &&
	grep "^\* feature-c" out &&
	! grep "^\* master" out
	)
'

test_expect_success 'checkout back to master' '
	(
	cd repo &&
	git checkout master &&
	git branch --show-current >out &&
	echo "master" >expected &&
	test_cmp expected out
	)
'

# ── Branch from tag ──────────────────────────────────────────────────────

test_expect_success 'branch: create branch from tag' '
	(
	cd repo &&
	git tag v1.0 HEAD~2 &&
	git branch from-tag v1.0 &&
	tag_oid=$(git rev-parse v1.0^{commit}) &&
	br_oid=$(git rev-parse from-tag) &&
	test "$tag_oid" = "$br_oid"
	)
'

# ── Verbose listing after operations ─────────────────────────────────────

test_expect_success 'branch -v: shows all branches after operations' '
	(
	cd repo &&
	git branch -v >out &&
	grep "master" out &&
	grep "feature-a" out &&
	grep "from-first" out &&
	grep "renamed" out
	)
'

test_expect_success 'branch: listing is alphabetically sorted' '
	(
	cd repo &&
	git branch >out &&
	sed "s/^[* ] //" out >names &&
	sort names >sorted &&
	test_cmp sorted names
	)
'

test_done
