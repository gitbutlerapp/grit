#!/bin/sh
# Tests for symbolic-ref create/read/delete and error cases.

test_description='symbolic-ref create, read, delete, and error cases'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

. ./test-lib.sh

GIT_AUTHOR_NAME='A U Thor'
GIT_AUTHOR_EMAIL='author@example.com'
GIT_COMMITTER_NAME='C O Mmiter'
GIT_COMMITTER_EMAIL='committer@example.com'
export GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL

test_expect_success 'setup: init repo with initial commit' '
	(
	git init repo &&
	cd repo &&
	echo "initial" >file.txt &&
	git add file.txt &&
	git commit -m "initial"
	)
'

test_expect_success 'HEAD is a symbolic ref' '
	(
	cd repo &&
	git symbolic-ref HEAD >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref reads HEAD correctly' '
	(
	cd repo &&
	ref=$(git symbolic-ref HEAD) &&
	test "$ref" = "refs/heads/master"
	)
'

test_expect_success '--short shows abbreviated ref' '
	(
	cd repo &&
	git symbolic-ref --short HEAD >actual &&
	echo "master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'create a new branch and switch HEAD' '
	(
	cd repo &&
	git update-ref refs/heads/feature $(git rev-parse HEAD) &&
	git symbolic-ref HEAD refs/heads/feature &&
	ref=$(git symbolic-ref HEAD) &&
	test "$ref" = "refs/heads/feature"
	)
'

test_expect_success '--short on feature branch' '
	(
	cd repo &&
	git symbolic-ref --short HEAD >actual &&
	echo "feature" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'switch back to master' '
	(
	cd repo &&
	git symbolic-ref HEAD refs/heads/master &&
	ref=$(git symbolic-ref HEAD) &&
	test "$ref" = "refs/heads/master"
	)
'

test_expect_success 'create custom symbolic ref' '
	(
	cd repo &&
	git symbolic-ref refs/custom/myref refs/heads/master &&
	ref=$(git symbolic-ref refs/custom/myref) &&
	test "$ref" = "refs/heads/master"
	)
'

test_expect_success 'custom symbolic ref resolves to same commit' '
	(
	cd repo &&
	custom_sha=$(git rev-parse refs/custom/myref) &&
	master_sha=$(git rev-parse refs/heads/master) &&
	test "$custom_sha" = "$master_sha"
	)
'

test_expect_success 'update custom symbolic ref to point elsewhere' '
	(
	cd repo &&
	git symbolic-ref refs/custom/myref refs/heads/feature &&
	ref=$(git symbolic-ref refs/custom/myref) &&
	test "$ref" = "refs/heads/feature"
	)
'

test_expect_success 'delete symbolic ref with -d' '
	(
	cd repo &&
	git symbolic-ref -d refs/custom/myref &&
	test_must_fail git symbolic-ref refs/custom/myref 2>/dev/null
	)
'

test_expect_success 'delete non-existent ref fails' '
	(
	cd repo &&
	test_must_fail git symbolic-ref -d refs/custom/nonexistent 2>err
	)
'

test_expect_success 'reading non-symbolic ref fails' '
	(
	cd repo &&
	test_must_fail git symbolic-ref refs/heads/master 2>err
	)
'

test_expect_success '-q suppresses error for non-symbolic ref' '
	(
	cd repo &&
	test_must_fail git symbolic-ref -q refs/heads/master 2>err &&
	test_must_be_empty err
	)
'

test_expect_success 'reading non-existent ref fails' '
	(
	cd repo &&
	test_must_fail git symbolic-ref refs/heads/nonexistent 2>err
	)
'

test_expect_success '-q on non-existent ref still fails' '
	(
	cd repo &&
	test_must_fail git symbolic-ref -q refs/heads/nonexistent 2>/dev/null
	)
'

test_expect_success 'symbolic-ref to deeply nested ref' '
	(
	cd repo &&
	git update-ref refs/deep/nested/branch $(git rev-parse HEAD) &&
	git symbolic-ref refs/sym/deep refs/deep/nested/branch &&
	ref=$(git symbolic-ref refs/sym/deep) &&
	test "$ref" = "refs/deep/nested/branch"
	)
'

test_expect_success 'deeply nested symbolic ref resolves' '
	(
	cd repo &&
	sha=$(git rev-parse refs/sym/deep) &&
	head_sha=$(git rev-parse HEAD) &&
	test "$sha" = "$head_sha"
	)
'

test_expect_success 'delete deeply nested symbolic ref' '
	(
	cd repo &&
	git symbolic-ref -d refs/sym/deep &&
	test_must_fail git symbolic-ref refs/sym/deep 2>/dev/null
	)
'

test_expect_success 'symbolic ref chain: A -> B -> commit' '
	(
	cd repo &&
	git symbolic-ref refs/chain/a refs/heads/master &&
	ref=$(git symbolic-ref refs/chain/a) &&
	test "$ref" = "refs/heads/master" &&
	sha=$(git rev-parse refs/chain/a) &&
	head_sha=$(git rev-parse HEAD) &&
	test "$sha" = "$head_sha"
	)
'

test_expect_success '--no-recurse stops after one level' '
	(
	cd repo &&
	git symbolic-ref refs/chain/b refs/chain/a &&
	ref=$(git symbolic-ref --no-recurse refs/chain/b) &&
	test "$ref" = "refs/chain/a"
	)
'

test_expect_success 'setup: create multiple branches' '
	(
	cd repo &&
	git update-ref refs/heads/branch1 $(git rev-parse HEAD) &&
	git update-ref refs/heads/branch2 $(git rev-parse HEAD) &&
	git update-ref refs/heads/branch3 $(git rev-parse HEAD)
	)
'

test_expect_success 'symbolic-ref to each branch in sequence' '
	(
	cd repo &&
	git symbolic-ref HEAD refs/heads/branch1 &&
	test "$(git symbolic-ref HEAD)" = "refs/heads/branch1" &&
	git symbolic-ref HEAD refs/heads/branch2 &&
	test "$(git symbolic-ref HEAD)" = "refs/heads/branch2" &&
	git symbolic-ref HEAD refs/heads/branch3 &&
	test "$(git symbolic-ref HEAD)" = "refs/heads/branch3"
	)
'

test_expect_success 'restore HEAD to master' '
	(
	cd repo &&
	git symbolic-ref HEAD refs/heads/master &&
	test "$(git symbolic-ref HEAD)" = "refs/heads/master"
	)
'

test_expect_success 'symbolic-ref with -m sets reflog message' '
	(
	cd repo &&
	git symbolic-ref -m "switching to feature" HEAD refs/heads/feature &&
	ref=$(git symbolic-ref HEAD) &&
	test "$ref" = "refs/heads/feature"
	)
'

test_expect_success 'setup: fresh repo for detached HEAD test' '
	(
	git init detached-repo &&
	cd detached-repo &&
	echo "data" >data.txt &&
	git add data.txt &&
	git commit -m "data" &&
	sha=$(git rev-parse HEAD) &&
	# Detach HEAD by pointing it at a SHA directly
	git update-ref --no-deref HEAD "$sha"
	)
'

test_expect_success 'symbolic-ref fails on detached HEAD' '
	(
	cd detached-repo &&
	test_must_fail git symbolic-ref HEAD 2>err
	)
'

test_expect_success 'reattach HEAD with symbolic-ref' '
	(
	cd detached-repo &&
	git symbolic-ref HEAD refs/heads/master &&
	ref=$(git symbolic-ref HEAD) &&
	test "$ref" = "refs/heads/master"
	)
'

test_expect_success 'setup: repo for edge cases' '
	(
	git init edge-repo &&
	cd edge-repo &&
	echo "edge" >edge.txt &&
	git add edge.txt &&
	git commit -m "edge"
	)
'

test_expect_success 'symbolic-ref to refs/tags namespace' '
	(
	cd edge-repo &&
	git update-ref refs/tags/v1 $(git rev-parse HEAD) &&
	git symbolic-ref refs/sym/tag-alias refs/tags/v1 &&
	ref=$(git symbolic-ref refs/sym/tag-alias) &&
	test "$ref" = "refs/tags/v1"
	)
'

test_expect_success 'symbolic-ref to refs/tags resolves correctly' '
	(
	cd edge-repo &&
	sha=$(git rev-parse refs/sym/tag-alias) &&
	tag_sha=$(git rev-parse refs/tags/v1) &&
	test "$sha" = "$tag_sha"
	)
'

test_expect_success 'delete symbolic ref to tag' '
	(
	cd edge-repo &&
	git symbolic-ref -d refs/sym/tag-alias &&
	test_must_fail git symbolic-ref refs/sym/tag-alias 2>/dev/null
	)
'

test_expect_success 'HEAD --short after checkout' '
	(
	cd edge-repo &&
	git symbolic-ref HEAD refs/heads/master &&
	short=$(git symbolic-ref --short HEAD) &&
	test "$short" = "master"
	)
'

test_done
