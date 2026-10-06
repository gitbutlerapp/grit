#!/bin/sh
# Tests merge between branches with divergent history

test_description='merge with divergent history'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup two diverged branches' '
	(
		git init ancestor-test &&
		cd ancestor-test &&
		git config user.name "Test" &&
		git config user.email "t@t.com" &&

		echo "base" >base &&
		git add base &&
		git commit -m "base" &&
		git tag base &&

		git branch left &&
		git branch right &&

		git checkout left &&
		echo "left" >left-file &&
		git add left-file &&
		git commit -m "left change" &&
		git tag left-tip &&

		git checkout right &&
		echo "right" >right-file &&
		git add right-file &&
		git commit -m "right change" &&
		git tag right-tip
	)
'

test_expect_success 'merge diverged branches succeeds without conflicts' '
	(
		cd ancestor-test &&
		git checkout left &&
		git merge right -m "merge right" &&
		test_path_is_file left-file &&
		test_path_is_file right-file &&
		test_path_is_file base
	)
'

test_expect_success 'merge-base of diverged branches is base' '
	(
		cd ancestor-test &&
		git merge-base left-tip right-tip >actual &&
		git rev-parse base >expect &&
		test_cmp expect actual
	)
'

test_done
