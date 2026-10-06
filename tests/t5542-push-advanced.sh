#!/bin/sh
# Tests for push :refspec (delete), push --mirror

test_description='push advanced: delete via :refspec, --mirror'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=main
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success setup '
	git init origin &&
	(
		cd origin &&
		echo one >file &&
		git add file &&
		git commit -m one &&
		git branch to-delete &&
		git branch keep
	) &&
	git clone origin local
'

test_expect_success 'push :branch deletes remote ref' '
	(
		cd local &&
		git push origin :to-delete
	) &&
	(
		cd origin &&
		test_must_fail git rev-parse --verify refs/heads/to-delete
	)
'

test_expect_success 'push --mirror mirrors all refs' '
	(
		cd local &&
		git branch mirror-branch &&
		git tag mirror-tag &&
		git push --mirror origin
	) &&
	(
		cd origin &&
		git rev-parse --verify refs/heads/mirror-branch &&
		git rev-parse --verify refs/tags/mirror-tag
	)
'

test_expect_success 'push --mirror removes refs not in source' '
	(
		cd local &&
		git branch -D keep 2>/dev/null || true &&
		git push --mirror origin
	) &&
	(
		cd origin &&
		test_must_fail git rev-parse --verify refs/heads/keep
	)
'

test_done
