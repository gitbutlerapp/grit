#!/bin/sh
#
# Tests for 'grit show-ref --verify' — exact ref lookup, --exists,
# --hash, --dereference, --quiet, and error cases.

test_description='grit show-ref --verify and related options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup: repo with branches and tags' '
	(
	$REAL_GIT init --initial-branch=master repo &&
	cd repo &&
	$REAL_GIT config user.name "Test User" &&
	$REAL_GIT config user.email "test@example.com" &&
	echo first >file &&
	$REAL_GIT add file &&
	test_tick &&
	$REAL_GIT commit -m "first commit" &&
	$REAL_GIT tag v1.0 &&
	$REAL_GIT tag -a v1.0-annotated -m "version 1.0" &&
	echo second >file &&
	$REAL_GIT add file &&
	test_tick &&
	$REAL_GIT commit -m "second commit" &&
	$REAL_GIT tag v2.0 &&
	$REAL_GIT branch feature
	)
'

# ---------------------------------------------------------------------------
# --verify with valid refs
# ---------------------------------------------------------------------------
test_expect_success 'show-ref --verify refs/heads/master succeeds' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/master >actual &&
	test -s actual
	)
'

test_expect_success 'show-ref --verify refs/heads/master shows correct SHA' '
	(
	cd repo &&
	expected=$($REAL_GIT rev-parse refs/heads/master) &&
	grit show-ref --verify refs/heads/master >actual &&
	grep "$expected" actual
	)
'

test_expect_success 'show-ref --verify refs/heads/feature succeeds' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/feature >actual &&
	test -s actual
	)
'

test_expect_success 'show-ref --verify refs/tags/v1.0 succeeds' '
	(
	cd repo &&
	grit show-ref --verify refs/tags/v1.0 >actual &&
	test -s actual
	)
'

test_expect_success 'show-ref --verify refs/tags/v2.0 succeeds' '
	(
	cd repo &&
	grit show-ref --verify refs/tags/v2.0 >actual &&
	test -s actual
	)
'

test_expect_success 'show-ref --verify refs/tags/v1.0-annotated succeeds' '
	(
	cd repo &&
	grit show-ref --verify refs/tags/v1.0-annotated >actual &&
	test -s actual
	)
'

# ---------------------------------------------------------------------------
# --verify with invalid/nonexistent refs
# ---------------------------------------------------------------------------
test_expect_success 'show-ref --verify fails for nonexistent ref' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify refs/heads/nonexistent 2>err
	)
'

test_expect_success 'show-ref --verify fails for bare branch name' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify master 2>err
	)
'

test_expect_success 'show-ref --verify fails for partial ref' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify heads/master 2>err
	)
'

# ---------------------------------------------------------------------------
# --verify with multiple refs
# ---------------------------------------------------------------------------
test_expect_success 'show-ref --verify with multiple valid refs' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/master refs/tags/v1.0 >actual &&
	grep "refs/heads/master" actual &&
	grep "refs/tags/v1.0" actual
	)
'

test_expect_success 'show-ref --verify with one valid one invalid fails' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify refs/heads/master refs/heads/nonexistent 2>err
	)
'

# ---------------------------------------------------------------------------
# --exists
# ---------------------------------------------------------------------------
test_expect_success 'show-ref --exists succeeds for existing ref' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/master
	)
'

test_expect_success 'show-ref --exists fails for nonexistent ref' '
	(
	cd repo &&
	test_must_fail grit show-ref --exists refs/heads/nonexistent
	)
'

test_expect_success 'show-ref --exists produces no output for existing ref' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/master >actual 2>&1 &&
	test_must_be_empty actual
	)
'

# ---------------------------------------------------------------------------
# --hash
# ---------------------------------------------------------------------------
test_expect_success 'show-ref --verify --hash shows only SHA' '
	(
	cd repo &&
	grit show-ref --verify --hash refs/heads/master >actual &&
	expected=$($REAL_GIT rev-parse refs/heads/master) &&
	echo "$expected" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --verify --hash for tag shows tag object SHA' '
	(
	cd repo &&
	grit show-ref --verify --hash refs/tags/v1.0 >actual &&
	expected=$($REAL_GIT rev-parse refs/tags/v1.0) &&
	echo "$expected" >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# --quiet
# ---------------------------------------------------------------------------
test_expect_success 'show-ref --verify --quiet produces no output on success' '
	(
	cd repo &&
	grit show-ref --verify --quiet refs/heads/master >actual 2>&1 &&
	test_must_be_empty actual
	)
'

test_expect_success 'show-ref --verify --quiet exits 0 for existing ref' '
	(
	cd repo &&
	grit show-ref --verify --quiet refs/heads/master
	)
'

test_expect_success 'show-ref --verify --quiet exits nonzero for missing ref' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify --quiet refs/heads/nonexistent
	)
'

# ---------------------------------------------------------------------------
# --dereference with annotated tags
# ---------------------------------------------------------------------------
test_expect_success 'show-ref --verify --dereference shows peeled tag' '
	(
	cd repo &&
	grit show-ref --verify --dereference refs/tags/v1.0-annotated >actual &&
	grep "refs/tags/v1.0-annotated$" actual &&
	grep "refs/tags/v1.0-annotated\^{}" actual
	)
'

test_expect_success 'show-ref --dereference peeled value matches commit SHA' '
	(
	cd repo &&
	grit show-ref --verify --dereference refs/tags/v1.0-annotated >actual &&
	commit_sha=$($REAL_GIT rev-parse v1.0-annotated^{commit}) &&
	grep "$commit_sha" actual
	)
'

# ---------------------------------------------------------------------------
# Compare with real git
# ---------------------------------------------------------------------------
test_expect_success 'show-ref --verify refs/heads/master matches real git' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/master >actual &&
	$REAL_GIT show-ref --verify refs/heads/master >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --verify refs/tags/v2.0 matches real git' '
	(
	cd repo &&
	grit show-ref --verify refs/tags/v2.0 >actual &&
	$REAL_GIT show-ref --verify refs/tags/v2.0 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --verify --hash matches real git' '
	(
	cd repo &&
	grit show-ref --verify --hash refs/heads/master >actual &&
	$REAL_GIT show-ref --verify --hash refs/heads/master >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --verify --dereference annotated tag matches real git' '
	(
	cd repo &&
	grit show-ref --verify --dereference refs/tags/v1.0-annotated >actual &&
	$REAL_GIT show-ref --verify --dereference refs/tags/v1.0-annotated >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --verify refs/tags/v1.0 matches real git' '
	(
	cd repo &&
	grit show-ref --verify refs/tags/v1.0 >actual &&
	$REAL_GIT show-ref --verify refs/tags/v1.0 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --verify refs/heads/feature matches real git' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/feature >actual &&
	$REAL_GIT show-ref --verify refs/heads/feature >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# Basic show-ref listing (no --verify)
# ---------------------------------------------------------------------------
test_expect_success 'show-ref without --verify lists all refs' '
	(
	cd repo &&
	grit show-ref >actual &&
	grep "refs/heads/master" actual &&
	grep "refs/tags/v1.0" actual
	)
'

test_expect_success 'show-ref --tags lists only tags' '
	(
	cd repo &&
	grit show-ref --tags >actual &&
	grep "refs/tags/" actual &&
	! grep "refs/heads/" actual
	)
'

test_expect_success 'show-ref --branches lists only branches' '
	(
	cd repo &&
	grit show-ref --branches >actual &&
	grep "refs/heads/" actual &&
	! grep "refs/tags/" actual
	)
'

test_done
