#!/bin/sh
# Adapted from git/t/t6434-merge-recursive-rename-options.sh
# Tests merge recursive with various options

test_description='merge recursive options'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=main
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: simple merge scenario' '
	git init merge-opts &&
	cd merge-opts &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&

	echo "line 1" >file1 &&
	echo "line 2" >file2 &&
	git add file1 file2 &&
	git commit -m "initial" &&

	git branch modify-A &&
	git branch modify-B &&

	git checkout modify-A &&
	echo "A change" >>file1 &&
	git add file1 &&
	git commit -m "modify file1 on A" &&

	git checkout modify-B &&
	echo "B change" >>file2 &&
	git add file2 &&
	git commit -m "modify file2 on B"
'

test_expect_success 'merge with non-overlapping changes' '
	cd merge-opts &&
	git checkout modify-A &&
	git merge modify-B -m "merge B" &&
	test_grep "A change" file1 &&
	test_grep "B change" file2
'

test_expect_success 'merge --no-commit stages but does not commit' '
	cd merge-opts &&
	git reset --hard modify-A &&
	git merge --no-commit modify-B &&
	# HEAD should still be at modify-A
	test "$(git rev-parse HEAD)" = "$(git rev-parse modify-A)" &&
	# but file2 should be merged
	test_grep "B change" file2
'

test_done
