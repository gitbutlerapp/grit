#!/bin/sh
# Tests for remote set-branches, remote prune, remote update

test_description='git remote subcommands: set-branches, prune, update'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=main
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success setup '
	git init upstream &&
	(
		cd upstream &&
		echo one >file &&
		git add file &&
		git commit -m one &&
		git branch feature &&
		git branch release
	) &&
	git clone upstream downstream
'

test_expect_success 'remote set-branches limits tracked branches' '
	(
		cd downstream &&
		git remote set-branches origin main &&
		git config --get-all remote.origin.fetch >actual &&
		echo "+refs/heads/main:refs/remotes/origin/main" >expect &&
		test_cmp expect actual
	)
'

test_expect_success 'remote set-branches --add appends' '
	(
		cd downstream &&
		git remote set-branches --add origin feature &&
		git config --get-all remote.origin.fetch >actual &&
		printf "+refs/heads/main:refs/remotes/origin/main\n+refs/heads/feature:refs/remotes/origin/feature\n" >expect &&
		test_cmp expect actual
	)
'

test_expect_success 'remote prune removes stale tracking refs' '
	(
		cd downstream &&
		git remote set-branches origin "main" "feature" "release" &&
		git fetch origin
	) &&
	(
		cd upstream &&
		git branch -D release
	) &&
	(
		cd downstream &&
		git remote prune origin 2>&1 | grep pruned &&
		test_must_fail git rev-parse --verify refs/remotes/origin/release
	)
'

test_expect_success 'remote update fetches from all remotes' '
	git init upstream2 &&
	(
		cd upstream2 &&
		echo two >file &&
		git add file &&
		git commit -m two
	) &&
	(
		cd downstream &&
		git remote add second ../upstream2 &&
		git remote update 2>&1 | grep "Fetching second" &&
		git rev-parse refs/remotes/second/main
	)
'

test_done
