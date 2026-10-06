#!/bin/sh
#
# Tests for 'switch --orphan'.
# Covers orphan branch creation, working tree clearing, and edge cases.

test_description='switch --orphan'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&

	echo first >first.t &&
	git add first.t &&
	git commit -m first &&
	git tag first &&

	git branch first-branch &&

	echo second >second.t &&
	git add second.t &&
	git commit -m second &&
	git tag second &&

	echo third >third.t &&
	git add third.t &&
	git commit -m third &&
	git tag third
	)
'

# ---------------------------------------------------------------------------
# switch --orphan creates new orphan branch with empty index
# ---------------------------------------------------------------------------
test_expect_success 'switch --orphan creates orphan with empty index' '
	(
	cd repo &&
	git switch master &&
	git switch --orphan new-orphan &&
	echo refs/heads/new-orphan >expect &&
	git symbolic-ref HEAD >actual &&
	test_cmp expect actual &&
	git ls-files >tracked &&
	test_must_be_empty tracked
	)
'

# ---------------------------------------------------------------------------
# switch --orphan with extra arg fails
# ---------------------------------------------------------------------------
test_expect_success 'switch --orphan with extra arg fails' '
	(
	cd repo &&
	git switch master &&
	test_must_fail git switch --orphan bad-orphan HEAD
	)
'

# ---------------------------------------------------------------------------
# orphan branch has no parent when committed
# ---------------------------------------------------------------------------
test_expect_success 'orphan branch commit has no parent' '
	(
	cd repo &&
	git switch master &&
	git switch --orphan commit-orphan &&
	echo orphan-content >orphan-file &&
	git add orphan-file &&
	git commit -m "orphan commit" &&
	git log --oneline >log &&
	test_line_count = 1 log
	)
'

# ---------------------------------------------------------------------------
# switch --discard-changes --orphan works with dirty worktree
# ---------------------------------------------------------------------------
test_expect_success 'switch --discard-changes --orphan with dirty worktree' '
	(
	cd repo &&
	git switch master &&
	echo dirty >first.t &&
	git switch --discard-changes --orphan discard-orphan &&
	git ls-files >tracked &&
	test_must_be_empty tracked &&
	echo refs/heads/discard-orphan >expect &&
	git symbolic-ref HEAD >actual &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# orphan branch does not have files from previous branch
# ---------------------------------------------------------------------------
test_expect_success 'orphan branch working tree has no tracked files' '
	(
	cd repo &&
	git switch master &&
	git switch --orphan clean-orphan &&
	# ls-files should be empty (no tracked files)
	git ls-files >tracked &&
	test_must_be_empty tracked
	)
'

# ---------------------------------------------------------------------------
# switching ignores file of same branch name
# (from t2060 but relevant for orphan context too)
# ---------------------------------------------------------------------------
test_expect_success 'switch back to master from orphan' '
	(
	cd repo &&
	git switch --orphan temp-orphan &&
	git switch master &&
	echo refs/heads/master >expect &&
	git symbolic-ref HEAD >actual &&
	test_cmp expect actual &&
	test -f first.t
	)
'

# ---------------------------------------------------------------------------
# Multiple orphan branches are independent
# ---------------------------------------------------------------------------
test_expect_success 'multiple orphan branches are independent' '
	(
	cd repo &&
	git switch master &&

	git switch --orphan orphan-a &&
	echo a >file-a &&
	git add file-a &&
	git commit -m "orphan a commit" &&
	oid_a=$(git rev-parse HEAD) &&

	git switch master &&
	git switch --orphan orphan-b &&
	echo b >file-b &&
	git add file-b &&
	git commit -m "orphan b commit" &&
	oid_b=$(git rev-parse HEAD) &&

	# They should have different trees and no common parent
	test "$oid_a" != "$oid_b" &&

	# orphan-a should only have file-a
	git switch orphan-a &&
	test -f file-a &&
	test_path_is_missing file-b &&

	# orphan-b should only have file-b
	git switch orphan-b &&
	test -f file-b &&
	test_path_is_missing file-a
	)
'

# ---------------------------------------------------------------------------
# switch --orphan from detached HEAD
# ---------------------------------------------------------------------------
test_expect_success 'switch --orphan from detached HEAD' '
	(
	cd repo &&
	git checkout --detach master &&
	test_must_fail git symbolic-ref HEAD &&
	git switch --orphan from-detached &&
	echo refs/heads/from-detached >expect &&
	git symbolic-ref HEAD >actual &&
	test_cmp expect actual &&
	git ls-files >tracked &&
	test_must_be_empty tracked
	)
'

# ---------------------------------------------------------------------------
# switch --orphan removes tracked files from worktree
# ---------------------------------------------------------------------------
test_expect_success 'switch --orphan removes tracked files from worktree' '
	(
	cd repo &&
	git switch master &&
	test -f first.t &&
	test -f second.t &&
	git switch --orphan wt-cleared &&
	test_path_is_missing first.t &&
	test_path_is_missing second.t &&
	test_path_is_missing third.t
	)
'

# ---------------------------------------------------------------------------
# switch --orphan preserves untracked files
# ---------------------------------------------------------------------------
test_expect_success 'switch --orphan preserves untracked files' '
	(
	cd repo &&
	git switch master &&
	echo untracked >untracked.t &&
	git switch --orphan keeps-untracked &&
	test -f untracked.t &&
	test "$(cat untracked.t)" = "untracked" &&
	rm -f untracked.t
	)
'

# ---------------------------------------------------------------------------
# switch --orphan with same name as existing branch fails
# ---------------------------------------------------------------------------
test_expect_success 'switch --orphan with existing branch name fails' '
	(
	cd repo &&
	git switch master &&
	test_must_fail git switch --orphan master 2>err &&
	test -s err
	)
'

# ---------------------------------------------------------------------------
# switch --orphan sets HEAD as symbolic ref
# ---------------------------------------------------------------------------
test_expect_success 'switch --orphan HEAD is unborn (rev-parse fails)' '
	(
	cd repo &&
	git switch master &&
	git switch --orphan unborn-check &&
	test_must_fail git rev-parse --verify HEAD 2>err
	)
'

# ---------------------------------------------------------------------------
# switch --orphan then switch back preserves master state
# ---------------------------------------------------------------------------
test_expect_success 'switch --orphan then switch back preserves master' '
	(
	cd repo &&
	git switch master &&
	master_oid=$(git rev-parse HEAD) &&
	git switch --orphan roundtrip-orphan &&
	git switch master &&
	test "$(git rev-parse HEAD)" = "$master_oid" &&
	test -f first.t &&
	test -f second.t &&
	test -f third.t
	)
'

# ---------------------------------------------------------------------------
# switch --orphan with -c flag is incompatible
# ---------------------------------------------------------------------------
test_expect_success 'switch --orphan with -c is rejected' '
	(
	cd repo &&
	git switch master &&
	test_must_fail git switch --orphan -c bad-combo 2>err
	)
'

test_done
