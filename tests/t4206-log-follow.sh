#!/bin/sh
# Tests for 'grit log' with graph display and branch topology.
# (--follow is not yet implemented; these tests cover --graph, --first-parent,
# and related topology display features.)

test_description='grit log graph and topology display'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup linear history' '
	(
	git init repo &&
	cd repo &&
	git config user.name "A U Thor" &&
	git config user.email "author@example.com" &&

	echo one >file &&
	git add file &&
	test_tick &&
	git commit -m "first" &&

	echo two >file &&
	git add file &&
	test_tick &&
	git commit -m "second" &&

	echo three >file &&
	git add file &&
	test_tick &&
	git commit -m "third"
	)
'

test_expect_success 'log --graph shows linear history' '
	(
	cd repo &&
	git log --graph --oneline --no-decorate >actual &&
	grep "third" actual &&
	grep "second" actual &&
	grep "first" actual
	)
'

test_expect_success 'log --graph --oneline outputs all commits' '
	(
	cd repo &&
	git log --graph --oneline --no-decorate >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'log --first-parent on linear history shows all' '
	(
	cd repo &&
	git log --first-parent --format="%s" >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'setup branch for diverge' '
	(
	cd repo &&
	SECOND=$(git rev-parse HEAD~1) &&
	git branch side "$SECOND" &&
	git checkout side &&
	echo side1 >side-file &&
	git add side-file &&
	test_tick &&
	git commit -m "side commit 1" &&

	echo side2 >side-file &&
	git add side-file &&
	test_tick &&
	git commit -m "side commit 2"
	)
'

test_expect_success 'log on side branch shows correct commits' '
	(
	cd repo &&
	git checkout side &&
	git log --format="%s" >actual &&
	head -1 actual >first_line &&
	echo "side commit 2" >expect &&
	test_cmp expect first_line &&
	test_line_count = 4 actual
	)
'

test_expect_success 'log --oneline on side branch' '
	(
	cd repo &&
	git checkout side &&
	git log --oneline --no-decorate >actual &&
	test_line_count = 4 actual
	)
'

test_expect_success 'log on main shows main commits' '
	(
	cd repo &&
	git checkout main &&
	git log --format="%s" >actual &&
	head -1 actual >first_line &&
	echo "third" >expect &&
	test_cmp expect first_line &&
	test_line_count = 3 actual
	)
'

test_expect_success 'log --graph on main is linear' '
	(
	cd repo &&
	git checkout main &&
	git log --graph --oneline --no-decorate >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'setup more branches' '
	(
	cd repo &&
	git checkout main &&
	git branch feature1 &&
	git branch feature2 &&
	git checkout feature1 &&
	echo f1 >f1 &&
	git add f1 &&
	test_tick &&
	git commit -m "feature1 work" &&

	git checkout feature2 &&
	echo f2 >f2 &&
	git add f2 &&
	test_tick &&
	git commit -m "feature2 work"
	)
'

test_expect_success 'log on feature1 shows feature commit' '
	(
	cd repo &&
	git checkout feature1 &&
	git log -n1 --format="%s" >actual &&
	echo "feature1 work" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log on feature2 shows feature commit' '
	(
	cd repo &&
	git checkout feature2 &&
	git log -n1 --format="%s" >actual &&
	echo "feature2 work" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --first-parent on feature branches' '
	(
	cd repo &&
	git checkout feature1 &&
	git log --first-parent --format="%s" >actual &&
	test_line_count = 4 actual
	)
'

test_expect_success 'log --graph on feature1 is linear' '
	(
	cd repo &&
	git checkout feature1 &&
	git log --graph --oneline --no-decorate >actual &&
	test_line_count = 4 actual
	)
'

test_expect_success 'log -n 1 on each branch shows different commits' '
	(
	cd repo &&
	git checkout main &&
	git log -n1 --format="%s" >main_tip &&
	git checkout feature1 &&
	git log -n1 --format="%s" >f1_tip &&
	git checkout feature2 &&
	git log -n1 --format="%s" >f2_tip &&
	! test_cmp main_tip f1_tip &&
	! test_cmp main_tip f2_tip &&
	! test_cmp f1_tip f2_tip
	)
'

test_expect_success 'log --reverse on feature1' '
	(
	cd repo &&
	git checkout feature1 &&
	git log --reverse --format="%s" >actual &&
	head -1 actual >first_line &&
	echo "first" >expect &&
	test_cmp expect first_line
	)
'

test_expect_success 'log --skip on feature branch' '
	(
	cd repo &&
	git checkout feature1 &&
	git log --skip=2 --format="%s" >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'log --skip combined with -n' '
	(
	cd repo &&
	git checkout feature1 &&
	git log --skip=1 -n 1 --format="%s" >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'log --decorate shows branch names' '
	(
	cd repo &&
	git checkout main &&
	git log -n1 --oneline --decorate >actual &&
	grep "main" actual
	)
'

test_expect_success 'log --no-decorate hides branch names' '
	(
	cd repo &&
	git checkout main &&
	git log -n1 --oneline --no-decorate >actual &&
	! grep "main" actual
	)
'

test_expect_success 'setup deep linear chain' '
	(
	cd repo &&
	git checkout main &&
	for i in $(seq 1 20); do
		echo "change $i" >file &&
		git add file &&
		test_tick &&
		git commit -m "chain commit $i" || return 1
	done
	)
'

test_expect_success 'log -n limits deep history' '
	(
	cd repo &&
	git log -n 5 --format="%s" >actual &&
	test_line_count = 5 actual
	)
'

test_expect_success 'log --skip on deep history' '
	(
	cd repo &&
	git log --skip=15 --format="%s" >actual &&
	test_line_count = 8 actual
	)
'

test_expect_success 'log --graph on deep linear history' '
	(
	cd repo &&
	git log --graph --oneline --no-decorate >actual &&
	test_line_count = 23 actual
	)
'

test_expect_success 'log --reverse on deep history starts with first' '
	(
	cd repo &&
	git log --reverse --format="%s" >actual &&
	head -1 actual >first_line &&
	echo "first" >expect &&
	test_cmp expect first_line
	)
'

test_expect_success 'log --first-parent on deep linear matches full log' '
	(
	cd repo &&
	git log --first-parent --format="%H" >fp &&
	git log --format="%H" >all &&
	test_cmp fp all
	)
'

test_done
