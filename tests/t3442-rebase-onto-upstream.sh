#!/bin/sh

test_description='grit rebase --onto A B with explicit upstream'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=main
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: divergent branches' '
	git init repo &&
	(
	cd repo &&
	echo base >file.txt &&
	git add file.txt &&
	git commit -m "base" &&
	BASE=$(git rev-parse HEAD) &&

	echo main1 >>file.txt &&
	git add file.txt &&
	git commit -m "main1" &&

	echo main2 >>file.txt &&
	git add file.txt &&
	git commit -m "main2" &&
	MAIN_TIP=$(git rev-parse HEAD) &&

	git checkout -b feature $BASE &&
	echo feat1 >feat.txt && git add feat.txt && git commit -m "feat1" &&
	echo feat2 >>feat.txt && git add feat.txt && git commit -m "feat2" &&

	git checkout -b newbase main &&
	echo newbase >nb.txt && git add nb.txt && git commit -m "newbase"
	)
'

test_expect_success 'rebase --onto newbase main feature replays feature onto newbase' '
	(
	cd repo &&
	git checkout feature &&
	git rebase --onto newbase main &&
	# feature should now be on top of newbase
	git log --format="%s" >../onto_log &&
	head -n 2 ../onto_log >../onto_top &&
	grep "feat2" ../onto_top &&
	grep "feat1" ../onto_top &&
	# newbase commit should be an ancestor
	grep "newbase" ../onto_log
	)
'

test_expect_success 'rebased commits have newbase as ancestor' '
	(
	cd repo &&
	git log --format="%s" | grep "newbase"
	)
'

test_done
