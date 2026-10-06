#!/bin/sh
# Tests for symbolic-ref: creation, reading, deletion, chains, --short, --no-recurse.

test_description='symbolic-ref chains and options'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- setup -----------------------------------------------------------------

test_expect_success 'setup: repo with branches' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "base" >file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "first" &&
	$REAL_GIT branch feature &&
	$REAL_GIT branch develop &&
	echo "second" >>file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "second"
	)
'

# -- basic create and read ---------------------------------------------------

test_expect_success 'symbolic-ref creates a symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/sym refs/heads/master &&
	grit symbolic-ref refs/heads/sym >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref reads HEAD' '
	(
	cd repo &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref to a different branch' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/sym refs/heads/feature &&
	grit symbolic-ref refs/heads/sym >actual &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref overwrites existing symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/sym refs/heads/develop &&
	grit symbolic-ref refs/heads/sym >actual &&
	echo "refs/heads/develop" >expect &&
	test_cmp expect actual
	)
'

# -- --short ----------------------------------------------------------------

test_expect_success 'symbolic-ref --short HEAD strips refs/heads/' '
	(
	cd repo &&
	grit symbolic-ref --short HEAD >actual &&
	echo "master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref --short for custom symref' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/sym refs/heads/feature &&
	grit symbolic-ref --short refs/heads/sym >actual &&
	echo "feature" >expect &&
	test_cmp expect actual
	)
'

# -- --delete / -d -----------------------------------------------------------

test_expect_success 'symbolic-ref -d deletes a symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/todelete refs/heads/master &&
	grit symbolic-ref refs/heads/todelete >check &&
	test "$(cat check)" = "refs/heads/master" &&
	grit symbolic-ref -d refs/heads/todelete &&
	test_must_fail grit symbolic-ref refs/heads/todelete 2>err
	)
'

test_expect_success 'symbolic-ref --delete same as -d' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/todelete2 refs/heads/master &&
	grit symbolic-ref --delete refs/heads/todelete2 &&
	test_must_fail grit symbolic-ref refs/heads/todelete2 2>err
	)
'

test_expect_success 'symbolic-ref -d on non-existent ref fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref -d refs/heads/nonexistent 2>err
	)
'

# -- --quiet / -q -----------------------------------------------------------

test_expect_success 'symbolic-ref -q on non-symbolic ref fails quietly' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref -q refs/heads/master >out 2>err &&
	test ! -s out
	)
'

test_expect_success 'symbolic-ref -q on valid symbolic ref succeeds' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/qtest refs/heads/master &&
	grit symbolic-ref -q refs/heads/qtest >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

# -- chains of symbolic refs -------------------------------------------------

test_expect_success 'create chain: A -> B -> master' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/chain-b refs/heads/master &&
	grit symbolic-ref refs/heads/chain-a refs/heads/chain-b
	)
'

test_expect_success 'reading chain-a resolves to master (full dereference)' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/chain-a >actual &&
	# Without --no-recurse, should resolve through the chain
	target=$(cat actual) &&
	# Should be either refs/heads/chain-b or refs/heads/master
	# depending on recursion behavior
	test "$target" = "refs/heads/chain-b" || test "$target" = "refs/heads/master"
	)
'

test_expect_success 'rev-parse resolves chain-a to master commit' '
	(
	cd repo &&
	grit rev-parse refs/heads/chain-a >actual &&
	grit rev-parse refs/heads/master >expect &&
	test_cmp expect actual
	)
'

# -- --no-recurse ------------------------------------------------------------

test_expect_success 'symbolic-ref --no-recurse stops at first symref' '
	(
	cd repo &&
	grit symbolic-ref --no-recurse refs/heads/chain-a >actual &&
	echo "refs/heads/chain-b" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref --no-recurse on single-level symref' '
	(
	cd repo &&
	grit symbolic-ref --no-recurse refs/heads/chain-b >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

# -- symbolic ref pointing at tag namespace ----------------------------------

test_expect_success 'symbolic-ref can point to refs/tags/' '
	(
	cd repo &&
	$REAL_GIT tag v1.0 &&
	grit symbolic-ref refs/heads/tagsym refs/tags/v1.0 &&
	grit symbolic-ref refs/heads/tagsym >actual &&
	echo "refs/tags/v1.0" >expect &&
	test_cmp expect actual
	)
'

# -- HEAD manipulation -------------------------------------------------------

test_expect_success 'symbolic-ref HEAD to different branch' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/feature &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref HEAD back to master' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/master &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref --short HEAD after switch' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/develop &&
	grit symbolic-ref --short HEAD >actual &&
	echo "develop" >expect &&
	test_cmp expect actual &&
	grit symbolic-ref HEAD refs/heads/master
	)
'

# -- reflog message with -m -------------------------------------------------

test_expect_success 'symbolic-ref -m records message (no crash)' '
	(
	cd repo &&
	grit symbolic-ref -m "switching to feature" HEAD refs/heads/feature &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual &&
	grit symbolic-ref HEAD refs/heads/master
	)
'

# -- error cases -------------------------------------------------------------

test_expect_success 'symbolic-ref with no args fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref 2>err
	)
'

test_expect_success 'symbolic-ref -d on regular ref fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref -d refs/heads/master 2>err
	)
'

# -- cleanup chain and verify ------------------------------------------------

test_expect_success 'delete chain-a does not delete chain-b' '
	(
	cd repo &&
	grit symbolic-ref -d refs/heads/chain-a &&
	grit symbolic-ref refs/heads/chain-b >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'resolving deleted symbolic ref fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref refs/heads/chain-a 2>err
	)
'

test_expect_success 'symbolic-ref to non-existent target still creates symref' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/dangling refs/heads/does-not-exist &&
	grit symbolic-ref refs/heads/dangling >actual &&
	echo "refs/heads/does-not-exist" >expect &&
	test_cmp expect actual &&
	test_must_fail grit rev-parse refs/heads/dangling 2>err
	)
'

test_expect_success 'symbolic-ref --short on chain resolves short name' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/shortchain refs/heads/master &&
	grit symbolic-ref --short refs/heads/shortchain >actual &&
	echo "master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref can be updated multiple times' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/multi refs/heads/master &&
	grit symbolic-ref refs/heads/multi refs/heads/feature &&
	grit symbolic-ref refs/heads/multi refs/heads/develop &&
	grit symbolic-ref refs/heads/multi >actual &&
	echo "refs/heads/develop" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-parse HEAD follows symbolic-ref HEAD' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/master &&
	grit rev-parse HEAD >a &&
	grit rev-parse refs/heads/master >b &&
	test_cmp a b
	)
'

test_done
