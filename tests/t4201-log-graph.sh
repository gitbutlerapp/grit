#!/bin/sh
test_description='grit log --graph, --first-parent, --reverse, --skip

Tests the graph display mode and commit traversal options for the log
command, including combinations of flags.'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup: linear history' '
	(
	git init linear &&
	cd linear &&
	git config user.name "Test User" &&
	git config user.email "test@test.com" &&
	echo "a" >file.txt && git add file.txt && git commit -m "first" &&
	echo "b" >file.txt && git add file.txt && git commit -m "second" &&
	echo "c" >file.txt && git add file.txt && git commit -m "third" &&
	echo "d" >file.txt && git add file.txt && git commit -m "fourth" &&
	echo "e" >file.txt && git add file.txt && git commit -m "fifth"
	)
'

# ── --graph ──────────────────────────────────────────────────────────────────

test_expect_success 'log --graph runs without error' '
	(
	cd linear &&
	git log --graph --oneline >out &&
	test -s out
	)
'

test_expect_success 'log --graph shows all commits' '
	(
	cd linear &&
	git log --graph --oneline >out &&
	test_line_count = 5 out
	)
'

test_expect_success 'log --graph with -n limits output' '
	(
	cd linear &&
	git log --graph --oneline -n 2 >out &&
	test_line_count = 2 out
	)
'

test_expect_success 'log --graph preserves commit order' '
	(
	cd linear &&
	git log --graph --oneline >out &&
	head -1 out | grep "fifth" &&
	tail -1 out | grep "first"
	)
'

# ── --reverse ────────────────────────────────────────────────────────────────

test_expect_success 'log --reverse shows oldest first' '
	(
	cd linear &&
	git log --reverse --oneline >out &&
	head -1 out | grep "first" &&
	tail -1 out | grep "fifth"
	)
'

test_expect_success 'log --reverse with -n limits correctly' '
	(
	cd linear &&
	git log --reverse --oneline -n 3 >out &&
	test_line_count = 3 out
	)
'

test_expect_success 'log --reverse first commit is root' '
	(
	cd linear &&
	git log --reverse --oneline >out &&
	head -1 out | grep "first"
	)
'

# ── --skip ───────────────────────────────────────────────────────────────────

test_expect_success 'log --skip 0 shows all commits' '
	(
	cd linear &&
	git log --skip 0 --oneline >out &&
	test_line_count = 5 out
	)
'

test_expect_success 'log --skip 2 skips two newest commits' '
	(
	cd linear &&
	git log --skip 2 --oneline >out &&
	test_line_count = 3 out &&
	head -1 out | grep "third"
	)
'

test_expect_success 'log --skip with -n combines correctly' '
	(
	cd linear &&
	git log --skip 1 -n 2 --oneline >out &&
	test_line_count = 2 out &&
	head -1 out | grep "fourth" &&
	tail -1 out | grep "third"
	)
'

test_expect_success 'log --skip past all commits shows nothing' '
	(
	cd linear &&
	git log --skip 100 --oneline >out &&
	test_must_be_empty out
	)
'

# ── --first-parent ───────────────────────────────────────────────────────────

test_expect_success 'log --first-parent on linear history shows all' '
	(
	cd linear &&
	git log --first-parent --oneline >out &&
	test_line_count = 5 out
	)
'

# ── Combinations ─────────────────────────────────────────────────────────────

test_expect_success 'log --graph --reverse is rejected' '
	(
	cd linear &&
	test_must_fail git log --graph --reverse --oneline >out 2>err &&
	grep "reverse.*graph\|graph.*reverse" err
	)
'

test_expect_success 'log --skip 1 --reverse' '
	(
	cd linear &&
	git log --skip 1 --reverse --oneline >out &&
	test_line_count = 4 out
	)
'

# ── Format with graph ────────────────────────────────────────────────────────

test_expect_success 'log --graph with --format shows custom format' '
	(
	cd linear &&
	git log --graph --format="%H %s" >out &&
	grep "fifth" out &&
	grep "first" out
	)
'

test_expect_success 'log --graph --oneline has short hashes' '
	(
	cd linear &&
	git log --graph --oneline >out &&
	# Each line contains a short hash
	head -1 out | grep "[0-9a-f]\{7\}"
	)
'

# ── Multiple revisions / ranges ─────────────────────────────────────────────

test_expect_success 'log with explicit HEAD' '
	(
	cd linear &&
	git log --oneline HEAD >out &&
	test_line_count = 5 out
	)
'

test_expect_success 'log with rev-parsed ancestor shows subset' '
	(
	cd linear &&
	anc=$(git rev-parse HEAD~2) &&
	git log --oneline "$anc" >out &&
	test_line_count = 3 out
	)
'

test_expect_success 'log with tag as revision' '
	(
	cd linear &&
	git tag v1.0 HEAD~3 &&
	git log --oneline v1.0 >out &&
	test_line_count = 2 out
	)
'

# ── Branch-specific log ─────────────────────────────────────────────────────

test_expect_success 'setup: create branch with extra commits' '
	(
	cd linear &&
	anc=$(git rev-parse HEAD~2) &&
	git branch side "$anc" &&
	git checkout side &&
	echo "side1" >side.txt && git add side.txt && git commit -m "side-one" &&
	echo "side2" >side.txt && git add side.txt && git commit -m "side-two" &&
	git checkout master
	)
'

test_expect_success 'log branch shows branch-specific history' '
	(
	cd linear &&
	git log --oneline side >out &&
	grep "side-two" out &&
	grep "side-one" out &&
	grep "third" out
	)
'

test_expect_success 'log --graph on branch' '
	(
	cd linear &&
	git log --graph --oneline side >out &&
	grep "side-two" out
	)
'

test_done
