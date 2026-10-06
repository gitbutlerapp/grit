#!/bin/sh
#
# Tests for 'grit merge-base' — finding common ancestors, --all, --is-ancestor,
# --octopus, --independent.

test_description='grit merge-base --all and related options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup: forked graph (no merges needed)
#
#   A --- B --- C  (master)
#    \
#     +-- D --- E  (side)
#
# ---------------------------------------------------------------------------
test_expect_success 'setup: forked history' '
	(
	git init --initial-branch=master repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo a >file &&
	git add file &&
	git commit -m A &&
	git tag A &&
	echo b >file &&
	git add file &&
	git commit -m B &&
	git tag B &&
	echo c >file &&
	git add file &&
	git commit -m C &&
	git tag C &&
	git checkout -b side A &&
	echo d >file2 &&
	git add file2 &&
	git commit -m D &&
	git tag D &&
	echo e >file2 &&
	git add file2 &&
	git commit -m E &&
	git tag E
	)
'

# ---------------------------------------------------------------------------
# Basic merge-base
# ---------------------------------------------------------------------------
test_expect_success 'merge-base of master and side is A (fork point)' '
	(
	cd repo &&
	expected=$(git rev-parse A) &&
	actual=$(grit merge-base master side) &&
	test "$expected" = "$actual"
	)
'

test_expect_success 'merge-base of two identical refs returns that commit' '
	(
	cd repo &&
	expected=$(git rev-parse master) &&
	actual=$(grit merge-base master master) &&
	test "$expected" = "$actual"
	)
'

test_expect_success 'merge-base with direct ancestor returns the ancestor' '
	(
	cd repo &&
	expected=$(git rev-parse A) &&
	actual=$(grit merge-base A C) &&
	test "$expected" = "$actual"
	)
'

test_expect_success 'merge-base of B and D is A' '
	(
	cd repo &&
	expected=$(git rev-parse A) &&
	actual=$(grit merge-base B D) &&
	test "$expected" = "$actual"
	)
'

test_expect_success 'merge-base of C and E is A' '
	(
	cd repo &&
	expected=$(git rev-parse A) &&
	actual=$(grit merge-base C E) &&
	test "$expected" = "$actual"
	)
'

test_expect_success 'merge-base is commutative' '
	(
	cd repo &&
	ab=$(grit merge-base A B) &&
	ba=$(grit merge-base B A) &&
	test "$ab" = "$ba"
	)
'

test_expect_success 'merge-base of parent and child returns parent' '
	(
	cd repo &&
	expected=$(git rev-parse A) &&
	actual=$(grit merge-base A B) &&
	test "$expected" = "$actual"
	)
'

# ---------------------------------------------------------------------------
# --all
# ---------------------------------------------------------------------------
test_expect_success 'merge-base --all returns all common ancestors' '
	(
	cd repo &&
	grit merge-base --all master side >actual &&
	test -s actual
	)
'

test_expect_success 'merge-base --all for simple fork has one result' '
	(
	cd repo &&
	grit merge-base --all B D >actual &&
	test_line_count = 1 actual &&
	expected=$(git rev-parse A) &&
	echo "$expected" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'merge-base --all master side has one result (A)' '
	(
	cd repo &&
	grit merge-base --all master side >actual &&
	test_line_count = 1 actual &&
	expected=$(git rev-parse A) &&
	echo "$expected" >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# --is-ancestor
# ---------------------------------------------------------------------------
test_expect_success '--is-ancestor A B succeeds (A is ancestor of B)' '
	(
	cd repo &&
	grit merge-base --is-ancestor A B
	)
'

test_expect_success '--is-ancestor A C succeeds' '
	(
	cd repo &&
	grit merge-base --is-ancestor A C
	)
'

test_expect_success '--is-ancestor A E succeeds' '
	(
	cd repo &&
	grit merge-base --is-ancestor A E
	)
'

test_expect_success '--is-ancestor B A fails (B is not ancestor of A)' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor B A
	)
'

test_expect_success '--is-ancestor C A fails' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor C A
	)
'

test_expect_success '--is-ancestor E A fails' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor E A
	)
'

test_expect_success '--is-ancestor same commit succeeds' '
	(
	cd repo &&
	grit merge-base --is-ancestor A A
	)
'

test_expect_success '--is-ancestor B D fails (parallel branches)' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor B D
	)
'

test_expect_success '--is-ancestor D B fails (parallel branches)' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor D B
	)
'

# ---------------------------------------------------------------------------
# --octopus
# ---------------------------------------------------------------------------
test_expect_success '--octopus with two refs same as basic merge-base' '
	(
	cd repo &&
	basic=$(grit merge-base B D) &&
	octopus=$(grit merge-base --octopus B D) &&
	test "$basic" = "$octopus"
	)
'

test_expect_success '--octopus with three refs finds common ancestor' '
	(
	cd repo &&
	result=$(grit merge-base --octopus B D E) &&
	expected=$(git rev-parse A) &&
	test "$result" = "$expected"
	)
'

# ---------------------------------------------------------------------------
# --independent
# ---------------------------------------------------------------------------
test_expect_success '--independent with unrelated tips returns both' '
	(
	cd repo &&
	grit merge-base --independent B D >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success '--independent with ancestor pair returns only descendant' '
	(
	cd repo &&
	grit merge-base --independent A B >actual &&
	test_line_count = 1 actual &&
	echo "$(git rev-parse B)" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--independent C E returns both (parallel tips)' '
	(
	cd repo &&
	grit merge-base --independent C E >actual &&
	test_line_count = 2 actual
	)
'

# ---------------------------------------------------------------------------
# Using OIDs directly
# ---------------------------------------------------------------------------
test_expect_success 'merge-base works with raw OIDs' '
	(
	cd repo &&
	oid_b=$(git rev-parse B) &&
	oid_d=$(git rev-parse D) &&
	result=$(grit merge-base "$oid_b" "$oid_d") &&
	expected=$(git rev-parse A) &&
	test "$result" = "$expected"
	)
'

# ---------------------------------------------------------------------------
# Error cases
# ---------------------------------------------------------------------------
test_expect_success 'merge-base with invalid ref fails' '
	(
	cd repo &&
	test_must_fail grit merge-base nonexistent master
	)
'

test_expect_success 'merge-base with single ref fails' '
	(
	cd repo &&
	test_must_fail grit merge-base master
	)
'

# ---------------------------------------------------------------------------
# Deeper chain
# ---------------------------------------------------------------------------
test_expect_success 'setup: extend master with more commits' '
	(
	cd repo &&
	git checkout master &&
	for i in 1 2 3 4 5; do
		echo "deep-$i" >"deep$i" &&
		git add "deep$i" &&
		git commit -m "deep $i" || return 1
	done &&
	git tag deep5
	)
'

test_expect_success 'merge-base deep5 and side is still A' '
	(
	cd repo &&
	result=$(grit merge-base deep5 side) &&
	expected=$(git rev-parse A) &&
	test "$result" = "$expected"
	)
'

test_expect_success '--is-ancestor C deep5 succeeds (linear chain)' '
	(
	cd repo &&
	grit merge-base --is-ancestor C deep5
	)
'

test_done
