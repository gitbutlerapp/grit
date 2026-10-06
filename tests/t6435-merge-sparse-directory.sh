#!/bin/sh
# Ported from git/t/t6435-merge-sparse.sh
# Tests merge with directory/file interactions

test_description='merge with sparse directory changes'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=main
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: merge with directory additions on both sides' '
	git init merge-dirs &&
	(
		cd merge-dirs &&
		git config user.name "Test" &&
		git config user.email "t@t.com" &&

		echo base >base &&
		git add base &&
		git commit -m "initial" &&

		git branch sideA &&
		git branch sideB &&

		git checkout sideA &&
		mkdir dirA &&
		echo "file in dirA" >dirA/file &&
		git add dirA/file &&
		git commit -m "add dirA" &&

		git checkout sideB &&
		mkdir dirB &&
		echo "file in dirB" >dirB/file &&
		git add dirB/file &&
		git commit -m "add dirB"
	)
'

test_expect_success 'merge brings in both directories' '
	(
		cd merge-dirs &&
		git checkout sideA &&
		git merge sideB -m "merge sideB" &&
		test_path_is_file dirA/file &&
		test_path_is_file dirB/file
	)
'

test_done
