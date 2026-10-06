#!/bin/sh
#
# Tests for 'grit symbolic-ref -d' — deleting symbolic refs,
# reading, creating, --short, --no-recurse, --quiet, and error cases.

test_description='grit symbolic-ref delete and related operations'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup: repo with branches' '
	(
	$REAL_GIT init --initial-branch=master repo &&
	cd repo &&
	$REAL_GIT config user.name "Test User" &&
	$REAL_GIT config user.email "test@example.com" &&
	echo first >file &&
	$REAL_GIT add file &&
	test_tick &&
	$REAL_GIT commit -m "first commit" &&
	$REAL_GIT branch feature &&
	$REAL_GIT branch develop &&
	$REAL_GIT branch release &&
	$REAL_GIT branch staging
	)
'

# ---------------------------------------------------------------------------
# Read HEAD symbolic ref
# ---------------------------------------------------------------------------
test_expect_success 'symbolic-ref HEAD reads current branch' '
	(
	cd repo &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref HEAD matches real git' '
	(
	cd repo &&
	grit symbolic-ref HEAD >actual &&
	$REAL_GIT symbolic-ref HEAD >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# --short
# ---------------------------------------------------------------------------
test_expect_success 'symbolic-ref --short HEAD shows short name' '
	(
	cd repo &&
	grit symbolic-ref --short HEAD >actual &&
	echo "master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref --short HEAD matches real git' '
	(
	cd repo &&
	grit symbolic-ref --short HEAD >actual &&
	$REAL_GIT symbolic-ref --short HEAD >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# Create symbolic ref
# ---------------------------------------------------------------------------
test_expect_success 'symbolic-ref creates a new symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/alias refs/heads/feature &&
	$REAL_GIT symbolic-ref refs/heads/alias >actual &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref can update existing symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/alias refs/heads/develop &&
	$REAL_GIT symbolic-ref refs/heads/alias >actual &&
	echo "refs/heads/develop" >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# Delete symbolic ref
# ---------------------------------------------------------------------------
test_expect_success 'symbolic-ref -d deletes a symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/alias refs/heads/feature &&
	grit symbolic-ref -d refs/heads/alias &&
	test_must_fail $REAL_GIT symbolic-ref refs/heads/alias 2>err
	)
'

test_expect_success 'deleted symbolic ref no longer appears in show-ref' '
	(
	cd repo &&
	grit show-ref >actual &&
	! grep "refs/heads/alias" actual
	)
'

test_expect_success 'symbolic-ref -d on non-symbolic ref fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref -d refs/heads/feature 2>err
	)
'

test_expect_success 'symbolic-ref -d on nonexistent ref fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref -d refs/heads/nonexistent 2>err
	)
'

# ---------------------------------------------------------------------------
# Create and delete multiple symbolic refs
# ---------------------------------------------------------------------------
test_expect_success 'create sym1 pointing to develop' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/sym1 refs/heads/develop &&
	grit symbolic-ref refs/heads/sym1 >actual &&
	echo "refs/heads/develop" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'create sym2 pointing to release' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/sym2 refs/heads/release &&
	grit symbolic-ref refs/heads/sym2 >actual &&
	echo "refs/heads/release" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'delete sym1' '
	(
	cd repo &&
	grit symbolic-ref -d refs/heads/sym1 &&
	test_must_fail grit symbolic-ref refs/heads/sym1 2>err
	)
'

test_expect_success 'delete sym2' '
	(
	cd repo &&
	grit symbolic-ref -d refs/heads/sym2 &&
	test_must_fail grit symbolic-ref refs/heads/sym2 2>err
	)
'

# ---------------------------------------------------------------------------
# --quiet
# ---------------------------------------------------------------------------
test_expect_success 'symbolic-ref --quiet suppresses error for non-symbolic ref' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref --quiet refs/heads/feature 2>err &&
	test_must_be_empty err
	)
'

test_expect_success 'symbolic-ref --quiet on HEAD still outputs target' '
	(
	cd repo &&
	grit symbolic-ref --quiet HEAD >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# --no-recurse
# ---------------------------------------------------------------------------
test_expect_success 'setup: chained symbolic refs' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/chain1 refs/heads/staging &&
	$REAL_GIT symbolic-ref refs/heads/chain2 refs/heads/chain1
	)
'

test_expect_success 'symbolic-ref --no-recurse stops at first level' '
	(
	cd repo &&
	grit symbolic-ref --no-recurse refs/heads/chain2 >actual &&
	echo "refs/heads/chain1" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref without --no-recurse resolves fully' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/chain2 >actual &&
	echo "refs/heads/staging" >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# Clean up chained refs
# ---------------------------------------------------------------------------
test_expect_success 'delete chain2 symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref -d refs/heads/chain2 &&
	test_must_fail grit symbolic-ref refs/heads/chain2 2>err
	)
'

test_expect_success 'delete chain1 symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref -d refs/heads/chain1 &&
	test_must_fail grit symbolic-ref refs/heads/chain1 2>err
	)
'

# ---------------------------------------------------------------------------
# -m reflog message
# ---------------------------------------------------------------------------
test_expect_success 'symbolic-ref -m records reflog message' '
	(
	cd repo &&
	grit symbolic-ref -m "switching alias" refs/heads/logged-sym refs/heads/feature &&
	grit symbolic-ref refs/heads/logged-sym >actual &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'clean up logged-sym' '
	(
	cd repo &&
	grit symbolic-ref -d refs/heads/logged-sym
	)
'

# ---------------------------------------------------------------------------
# HEAD manipulation
# ---------------------------------------------------------------------------
test_expect_success 'symbolic-ref HEAD can be changed' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/feature &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'restore HEAD to master' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/master &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# Compare with real git
# ---------------------------------------------------------------------------
test_expect_success 'symbolic-ref create matches real git read' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/test-sym refs/heads/develop &&
	$REAL_GIT symbolic-ref refs/heads/test-sym >actual &&
	echo "refs/heads/develop" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'clean up test-sym' '
	(
	cd repo &&
	grit symbolic-ref -d refs/heads/test-sym
	)
'

test_done
