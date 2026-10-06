#!/bin/sh
#
# Tests for 'grit check-ref-format --branch' — branch shorthand expansion,
# plus --normalize, --allow-onelevel, --refspec-pattern, and basic validation.

test_description='grit check-ref-format --branch and validation'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

# ---------------------------------------------------------------------------
# Setup (needed for @{-N} expansion)
# ---------------------------------------------------------------------------
test_expect_success 'setup: repo with branch history' '
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
	$REAL_GIT checkout feature &&
	$REAL_GIT checkout develop &&
	$REAL_GIT checkout master
	)
'

# ---------------------------------------------------------------------------
# --branch with simple names
# ---------------------------------------------------------------------------
test_expect_success 'check-ref-format --branch master outputs master' '
	(
	cd repo &&
	grit check-ref-format --branch master >actual &&
	echo "master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'check-ref-format --branch feature outputs feature' '
	(
	cd repo &&
	grit check-ref-format --branch feature >actual &&
	echo "feature" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'check-ref-format --branch my-branch outputs my-branch' '
	(
	cd repo &&
	grit check-ref-format --branch my-branch >actual &&
	echo "my-branch" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'check-ref-format --branch topic/sub outputs topic/sub' '
	(
	cd repo &&
	grit check-ref-format --branch topic/sub >actual &&
	echo "topic/sub" >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# --branch rejects invalid names
# ---------------------------------------------------------------------------
test_expect_success 'check-ref-format --branch rejects name with ..' '
	(
	cd repo &&
	test_must_fail grit check-ref-format --branch "foo..bar" 2>err
	)
'

test_expect_success 'check-ref-format --branch rejects name with space' '
	(
	cd repo &&
	test_must_fail grit check-ref-format --branch "foo bar" 2>err
	)
'

test_expect_success 'check-ref-format --branch rejects name with ~' '
	(
	cd repo &&
	test_must_fail grit check-ref-format --branch "foo~1" 2>err
	)
'

test_expect_success 'check-ref-format --branch rejects name with ^' '
	(
	cd repo &&
	test_must_fail grit check-ref-format --branch "foo^bar" 2>err
	)
'

test_expect_success 'check-ref-format --branch rejects name with colon' '
	(
	cd repo &&
	test_must_fail grit check-ref-format --branch "foo:bar" 2>err
	)
'

test_expect_success 'check-ref-format --branch rejects name ending with .lock' '
	(
	cd repo &&
	test_must_fail grit check-ref-format --branch "foo.lock" 2>err
	)
'

test_expect_success 'check-ref-format --branch rejects @{' '
	(
	cd repo &&
	test_must_fail grit check-ref-format --branch "foo@{bar" 2>err
	)
'

test_expect_success 'check-ref-format --branch rejects backslash' '
	(
	cd repo &&
	test_must_fail grit check-ref-format --branch "foo\\bar" 2>err
	)
'

# ---------------------------------------------------------------------------
# Basic check-ref-format (no --branch)
# ---------------------------------------------------------------------------
test_expect_success 'check-ref-format accepts refs/heads/master' '
	grit check-ref-format refs/heads/master
'

test_expect_success 'check-ref-format accepts refs/tags/v1.0' '
	grit check-ref-format refs/tags/v1.0
'

test_expect_success 'check-ref-format accepts refs/remotes/origin/main' '
	grit check-ref-format refs/remotes/origin/main
'

test_expect_success 'check-ref-format rejects bare name (no slash)' '
	test_must_fail grit check-ref-format master
'

test_expect_success 'check-ref-format rejects double dots' '
	test_must_fail grit check-ref-format "refs/heads/foo..bar"
'

test_expect_success 'check-ref-format rejects trailing slash' '
	test_must_fail grit check-ref-format "refs/heads/foo/"
'

test_expect_success 'check-ref-format rejects leading dot component' '
	test_must_fail grit check-ref-format "refs/heads/.hidden"
'

test_expect_success 'check-ref-format rejects .lock suffix' '
	test_must_fail grit check-ref-format "refs/heads/foo.lock"
'

# ---------------------------------------------------------------------------
# --allow-onelevel
# ---------------------------------------------------------------------------
test_expect_success 'check-ref-format --allow-onelevel accepts HEAD' '
	grit check-ref-format --allow-onelevel HEAD
'

test_expect_success 'check-ref-format --allow-onelevel accepts MERGE_HEAD' '
	grit check-ref-format --allow-onelevel MERGE_HEAD
'

test_expect_success 'check-ref-format --allow-onelevel accepts single name' '
	grit check-ref-format --allow-onelevel master
'

test_expect_success 'check-ref-format --allow-onelevel still rejects invalid chars' '
	test_must_fail grit check-ref-format --allow-onelevel "foo bar"
'

# ---------------------------------------------------------------------------
# --refspec-pattern
# ---------------------------------------------------------------------------
test_expect_success 'check-ref-format --refspec-pattern accepts single wildcard' '
	grit check-ref-format --refspec-pattern "refs/heads/*"
'

test_expect_success 'check-ref-format --refspec-pattern accepts wildcard in middle' '
	grit check-ref-format --refspec-pattern "refs/*/master"
'

test_expect_success 'check-ref-format --refspec-pattern rejects double wildcard' '
	test_must_fail grit check-ref-format --refspec-pattern "refs/heads/*/*"
'

# ---------------------------------------------------------------------------
# --normalize
# ---------------------------------------------------------------------------
test_expect_success 'check-ref-format --normalize strips leading slash' '
	grit check-ref-format --normalize "/refs/heads/master" >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
'

test_expect_success 'check-ref-format --normalize collapses double slashes' '
	grit check-ref-format --normalize "refs//heads///master" >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
'

test_expect_success 'check-ref-format --normalize still rejects invalid' '
	test_must_fail grit check-ref-format --normalize "refs/heads/foo..bar" 2>err
'

# ---------------------------------------------------------------------------
# Compare with real git
# ---------------------------------------------------------------------------
test_expect_success 'check-ref-format --branch master matches real git' '
	(
	cd repo &&
	grit check-ref-format --branch master >actual &&
	$REAL_GIT check-ref-format --branch master >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'check-ref-format --normalize /refs/heads/x matches real git' '
	grit check-ref-format --normalize "/refs/heads/x" >actual &&
	$REAL_GIT check-ref-format --normalize "/refs/heads/x" >expect &&
	test_cmp expect actual
'

test_expect_success 'check-ref-format --normalize refs//heads//x matches real git' '
	grit check-ref-format --normalize "refs//heads//x" >actual &&
	$REAL_GIT check-ref-format --normalize "refs//heads//x" >expect &&
	test_cmp expect actual
'

test_done
