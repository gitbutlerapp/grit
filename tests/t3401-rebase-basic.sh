#!/bin/sh
#
# Tests for cherry-pick sequences simulating rebase-like workflows.
# grit does not have native 'rebase' but cherry-pick (passthrough) is
# available. These tests exercise sequential cherry-pick application,
# squash mode (--no-commit), author preservation, and grit log/diff/show
# on cherry-picked results.

test_description='grit cherry-pick sequences — rebase-like workflows'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup: linear history with divergent branch
# ---------------------------------------------------------------------------
test_expect_success 'setup base repository with linear history' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&

	echo "base" >file.txt &&
	git add file.txt &&
	git commit -m "base" &&
	git rev-parse HEAD >../base_sha &&

	echo "line 2" >>file.txt &&
	git add file.txt &&
	git commit -m "main-A" &&
	git rev-parse HEAD >../mainA &&

	echo "line 3" >>file.txt &&
	git add file.txt &&
	git commit -m "main-B" &&
	git rev-parse HEAD >../mainB
	)
'

test_expect_success 'setup independent branch with non-conflicting commits' '
	(
	cd repo &&
	git checkout -b independent $(cat ../base_sha) &&
	echo "ind-A" >ind.txt &&
	git add ind.txt &&
	git commit -m "ind-A" &&
	git rev-parse HEAD >../indA &&

	echo "ind-B" >>ind.txt &&
	git add ind.txt &&
	git commit -m "ind-B" &&
	git rev-parse HEAD >../indB &&

	echo "ind-C" >>ind.txt &&
	git add ind.txt &&
	git commit -m "ind-C" &&
	git rev-parse HEAD >../indC
	)
'

# ---------------------------------------------------------------------------
# Single cherry-pick
# ---------------------------------------------------------------------------
test_expect_success 'cherry-pick single commit onto master' '
	(
	cd repo &&
	git checkout master &&
	git cherry-pick $(cat ../indA) &&
	test -f ind.txt &&
	grep "ind-A" ind.txt
	)
'

test_expect_success 'cherry-picked commit has different SHA from original' '
	(
	cd repo &&
	picked=$(git rev-parse HEAD) &&
	test "$picked" != "$(cat ../indA)"
	)
'

test_expect_success 'cherry-pick preserves commit message' '
	(
	cd repo &&
	git log -n 1 --format="%s" >../msg &&
	grep "ind-A" ../msg
	)
'

test_expect_success 'original file.txt unchanged after cherry-pick' '
	(
	cd repo &&
	grep "line 3" file.txt
	)
'

# ---------------------------------------------------------------------------
# Sequential cherry-pick (manual rebase of 3 commits)
# ---------------------------------------------------------------------------
test_expect_success 'setup branch for sequential cherry-pick' '
	(
	cd repo &&
	git checkout master &&
	git reset --hard $(cat ../mainB) &&
	git checkout -b rebased
	)
'

test_expect_success 'cherry-pick first of three commits' '
	(
	cd repo &&
	git checkout rebased &&
	git cherry-pick $(cat ../indA)
	)
'

test_expect_success 'cherry-pick second commit' '
	(
	cd repo &&
	git cherry-pick $(cat ../indB)
	)
'

test_expect_success 'cherry-pick third commit' '
	(
	cd repo &&
	git cherry-pick $(cat ../indC)
	)
'

test_expect_success 'all three subjects appear in log' '
	(
	cd repo &&
	git log --format="%s" >../seq_log &&
	grep "ind-A" ../seq_log &&
	grep "ind-B" ../seq_log &&
	grep "ind-C" ../seq_log
	)
'

test_expect_success 'file content matches sequential application' '
	(
	cd repo &&
	echo "ind-A" >../expected_ind &&
	echo "ind-B" >>../expected_ind &&
	echo "ind-C" >>../expected_ind &&
	test_cmp ../expected_ind ind.txt
	)
'

test_expect_success 'each cherry-picked commit got a unique new SHA' '
	(
	cd repo &&
	new3=$(git rev-parse HEAD) &&
	new2=$(git rev-parse HEAD~1) &&
	new1=$(git rev-parse HEAD~2) &&
	test "$new1" != "$(cat ../indA)" &&
	test "$new2" != "$(cat ../indB)" &&
	test "$new3" != "$(cat ../indC)"
	)
'

test_expect_success 'main-B is ancestor of rebased tip' '
	(
	cd repo &&
	git merge-base --is-ancestor $(cat ../mainB) HEAD
	)
'

test_expect_success 'rebased branch has correct total commit count' '
	(
	cd repo &&
	git log --oneline >../rebased_log &&
	test_line_count = 6 ../rebased_log
	)
'

# ---------------------------------------------------------------------------
# Cherry-pick --no-commit (squash mode)
# ---------------------------------------------------------------------------
test_expect_success 'setup branch for squash cherry-pick' '
	(
	cd repo &&
	git checkout master &&
	git reset --hard $(cat ../mainB) &&
	git checkout -b squash-test
	)
'

test_expect_success 'cherry-pick --no-commit stages without committing' '
	(
	cd repo &&
	git checkout squash-test &&
	git cherry-pick --no-commit $(cat ../indA) &&
	git status --porcelain >../st_out &&
	grep "ind.txt" ../st_out
	)
'

test_expect_success 'manual commit after --no-commit with custom message' '
	(
	cd repo &&
	git commit -m "squashed: ind-A" &&
	git log -n 1 --format="%s" >../sq_msg &&
	grep "squashed: ind-A" ../sq_msg
	)
'

test_expect_success 'squashed commit has only one log entry for the pick' '
	(
	cd repo &&
	git log --oneline >../sq_log &&
	test_line_count = 4 ../sq_log
	)
'

# ---------------------------------------------------------------------------
# Cherry-pick onto orphan branch
# ---------------------------------------------------------------------------
test_expect_success 'create orphan branch with separate file' '
	(
	cd repo &&
	git checkout master &&
	git checkout --orphan orphan-branch &&
	git rm -f file.txt &&
	echo "orphan" >orphan.txt &&
	git add orphan.txt &&
	git commit -m "orphan base"
	)
'

test_expect_success 'cherry-pick onto orphan adds new file' '
	(
	cd repo &&
	git cherry-pick $(cat ../indA) &&
	test -f ind.txt &&
	test -f orphan.txt
	)
'

test_expect_success 'orphan branch log has two commits' '
	(
	cd repo &&
	git log --oneline >../orph_log &&
	test_line_count = 2 ../orph_log
	)
'

# ---------------------------------------------------------------------------
# Diff between original and cherry-picked content
# ---------------------------------------------------------------------------
test_expect_success 'diff of cherry-picked file vs original is empty' '
	(
	cd repo &&
	git checkout rebased &&
	git diff $(cat ../indC) HEAD -- ind.txt >../cpd &&
	test_must_be_empty ../cpd
	)
'

test_expect_success 'diff --stat between main-B and rebased shows ind.txt' '
	(
	cd repo &&
	git diff --stat $(cat ../mainB) HEAD >../cp_stat &&
	grep "ind.txt" ../cp_stat
	)
'

test_expect_success 'diff --name-only between main-B and rebased' '
	(
	cd repo &&
	git diff --name-only $(cat ../mainB) HEAD >../cp_names &&
	grep "ind.txt" ../cp_names
	)
'

# ---------------------------------------------------------------------------
# Cherry-pick preserves author information
# ---------------------------------------------------------------------------
test_expect_success 'cherry-pick preserves author name' '
	(
	cd repo &&
	git checkout rebased &&
	git log -n 1 --format="%an" >../auth_name &&
	grep "A U Thor" ../auth_name
	)
'

test_expect_success 'cherry-pick preserves author email' '
	(
	cd repo &&
	git log -n 1 --format="%ae" >../auth_email &&
	grep "author@example.com" ../auth_email
	)
'

# ---------------------------------------------------------------------------
# Show on cherry-picked commits
# ---------------------------------------------------------------------------
test_expect_success 'grit show on cherry-picked commit displays diff' '
	(
	cd repo &&
	git show HEAD >../show_out &&
	grep "ind.txt" ../show_out
	)
'

test_expect_success 'grit show --quiet suppresses diff' '
	(
	cd repo &&
	git show --quiet HEAD >../show_q &&
	! grep "^diff" ../show_q &&
	grep "ind-C" ../show_q
	)
'

# ---------------------------------------------------------------------------
# Multiple sequential picks accumulate correctly
# ---------------------------------------------------------------------------
test_expect_success 'setup fresh branch for accumulation' '
	(
	cd repo &&
	git checkout master &&
	git reset --hard $(cat ../mainB) &&
	git checkout -b accum
	)
'

test_expect_success 'accumulate three cherry-picks and verify commit count' '
	(
	cd repo &&
	git checkout accum &&
	git cherry-pick $(cat ../indA) &&
	git cherry-pick $(cat ../indB) &&
	git cherry-pick $(cat ../indC) &&
	git log --oneline >../accum_log &&
	test_line_count = 6 ../accum_log
	)
'

test_expect_success 'rev-list counts match after accumulation' '
	(
	cd repo &&
	git rev-list HEAD >../revs &&
	test_line_count = 6 ../revs
	)
'

test_done
