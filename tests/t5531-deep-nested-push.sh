#!/bin/sh
# Ported from git/t/t5531 concept
# Tests push with nested directory structures

test_description='test push with nested repos'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=main
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup nested directories with commits' '
	git init -q &&
	mkdir -p a/b/c &&
	echo content >a/b/c/file &&
	git add a/b/c/file &&
	git commit -m "deeply nested file"
'

test_expect_success 'clone preserves nested structure' '
	git clone . clone &&
	test -f clone/a/b/c/file &&
	test content = "$(cat clone/a/b/c/file)"
'

test_expect_success 'push changes in nested directory' '
	git config receive.denyCurrentBranch warn &&
	(
		cd clone &&
		echo updated >a/b/c/file &&
		git add a/b/c/file &&
		git commit -m "update deeply nested" &&
		git push origin main
	) &&
	clone_head=$(cd clone && git rev-parse main) &&
	local_head=$(git rev-parse main) &&
	test "$clone_head" = "$local_head"
'

test_expect_success 'send-pack with nested content' '
	git init --bare dest.git &&
	git send-pack ./dest.git main &&
	git --git-dir=dest.git rev-parse main
'

test_done
