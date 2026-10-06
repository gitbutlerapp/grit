#!/bin/sh
# Tests for grit check-ref-format: valid/invalid refnames, --normalize,
# --allow-onelevel, --refspec-pattern, and --branch options.

test_description='grit check-ref-format validation and normalize options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository for --branch tests' '
	(
	grit init --initial-branch=master repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo x >x.txt &&
	grit add . &&
	grit commit -m "initial"
	)
'

###########################################################################
# Section 2: Valid refnames
###########################################################################

test_expect_success 'valid: refs/heads/master' '
	grit check-ref-format refs/heads/master
'

test_expect_success 'valid: refs/heads/feature-branch' '
	grit check-ref-format refs/heads/feature-branch
'

test_expect_success 'valid: refs/tags/v1.0' '
	grit check-ref-format refs/tags/v1.0
'

test_expect_success 'valid: refs/heads/a/b/c' '
	grit check-ref-format refs/heads/a/b/c
'

test_expect_success 'valid: refs/heads/UPPER' '
	grit check-ref-format refs/heads/UPPER
'

test_expect_success 'valid: refs/heads/mix3d-CaSe_under' '
	grit check-ref-format refs/heads/mix3d-CaSe_under
'

test_expect_success 'valid: refs/remotes/origin/main' '
	grit check-ref-format refs/remotes/origin/main
'

test_expect_success 'valid: refs/heads/a.b' '
	grit check-ref-format refs/heads/a.b
'

###########################################################################
# Section 3: Invalid refnames
###########################################################################

test_expect_success 'invalid: component starting with dot' '
	test_must_fail grit check-ref-format refs/heads/.hidden
'

test_expect_success 'invalid: double dot' '
	test_must_fail grit check-ref-format refs/heads/a..b
'

test_expect_success 'invalid: ends with .lock' '
	test_must_fail grit check-ref-format refs/heads/a.lock
'

test_expect_success 'invalid: contains space' '
	test_must_fail grit check-ref-format "refs/heads/a b"
'

test_expect_success 'invalid: contains tilde' '
	test_must_fail grit check-ref-format "refs/heads/a~b"
'

test_expect_success 'invalid: contains caret' '
	test_must_fail grit check-ref-format "refs/heads/a^b"
'

test_expect_success 'invalid: contains colon' '
	test_must_fail grit check-ref-format "refs/heads/a:b"
'

test_expect_success 'invalid: contains backslash' '
	test_must_fail grit check-ref-format "refs/heads/a\\b"
'

test_expect_success 'invalid: contains open bracket' '
	test_must_fail grit check-ref-format "refs/heads/a[b"
'

test_expect_success 'invalid: contains question mark' '
	test_must_fail grit check-ref-format "refs/heads/a?b"
'

test_expect_success 'invalid: contains asterisk without --refspec-pattern' '
	test_must_fail grit check-ref-format "refs/heads/*"
'

test_expect_success 'invalid: ends with slash' '
	test_must_fail grit check-ref-format "refs/heads/trail/"
'

test_expect_success 'invalid: single level without --allow-onelevel' '
	test_must_fail grit check-ref-format master
'

test_expect_success 'invalid: at-open-brace sequence' '
	test_must_fail grit check-ref-format "refs/heads/a@{b"
'

test_expect_success 'invalid: component is .lock' '
	test_must_fail grit check-ref-format "refs/heads/.lock"
'

###########################################################################
# Section 4: --allow-onelevel
###########################################################################

test_expect_success 'allow-onelevel: master is valid' '
	grit check-ref-format --allow-onelevel master
'

test_expect_success 'allow-onelevel: HEAD is valid' '
	grit check-ref-format --allow-onelevel HEAD
'

test_expect_success 'allow-onelevel: multi-level still valid' '
	grit check-ref-format --allow-onelevel refs/heads/master
'

test_expect_success 'allow-onelevel: invalid chars still rejected' '
	test_must_fail grit check-ref-format --allow-onelevel "a b"
'

test_expect_success 'allow-onelevel: double dot still rejected' '
	test_must_fail grit check-ref-format --allow-onelevel "a..b"
'

###########################################################################
# Section 5: --refspec-pattern
###########################################################################

test_expect_success 'refspec-pattern: single star is valid' '
	grit check-ref-format --refspec-pattern "refs/heads/*"
'

test_expect_success 'refspec-pattern: star in middle is valid' '
	grit check-ref-format --refspec-pattern "refs/*/master"
'

test_expect_success 'refspec-pattern: two stars is invalid' '
	test_must_fail grit check-ref-format --refspec-pattern "refs/heads/*/*"
'

test_expect_success 'refspec-pattern: without star is still valid' '
	grit check-ref-format --refspec-pattern refs/heads/master
'

###########################################################################
# Section 6: --normalize
###########################################################################

test_expect_success 'normalize: strips leading slash' '
	grit check-ref-format --normalize "/refs/heads/master" >actual &&
	echo "refs/heads/master" >expected &&
	test_cmp expected actual
'

test_expect_success 'normalize: collapses consecutive slashes' '
	grit check-ref-format --normalize "refs/heads//feature//test" >actual &&
	echo "refs/heads/feature/test" >expected &&
	test_cmp expected actual
'

test_expect_success 'normalize: strips leading and collapses' '
	grit check-ref-format --normalize "/refs///heads///master" >actual &&
	echo "refs/heads/master" >expected &&
	test_cmp expected actual
'

test_expect_success 'normalize: already normalized ref unchanged' '
	grit check-ref-format --normalize "refs/heads/master" >actual &&
	echo "refs/heads/master" >expected &&
	test_cmp expected actual
'

test_expect_success 'normalize: deeply nested path' '
	grit check-ref-format --normalize "refs//a//b//c//d" >actual &&
	echo "refs/a/b/c/d" >expected &&
	test_cmp expected actual
'

test_expect_success 'normalize: rejects invalid ref even after normalization' '
	test_must_fail grit check-ref-format --normalize "refs/heads/a..b"
'

test_expect_success 'normalize: rejects dot-started component' '
	test_must_fail grit check-ref-format --normalize "refs/heads/.bad"
'

###########################################################################
# Section 7: --branch
###########################################################################

test_expect_success 'branch: master expands to master' '
	(
	cd repo &&
	grit check-ref-format --branch master >actual &&
	echo "master" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'branch: feature-branch is valid' '
	(
	cd repo &&
	grit check-ref-format --branch feature-branch >actual &&
	echo "feature-branch" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'branch: invalid branch name fails' '
	(
	cd repo &&
	test_must_fail grit check-ref-format --branch "a..b"
	)
'

###########################################################################
# Section 8: Cross-check with real git
###########################################################################

test_expect_success 'cross-check: valid ref matches real git exit code' '
	grit check-ref-format refs/heads/master &&
	$REAL_GIT check-ref-format refs/heads/master
'

test_expect_success 'cross-check: invalid double-dot both fail' '
	test_must_fail grit check-ref-format refs/heads/a..b &&
	test_must_fail $REAL_GIT check-ref-format refs/heads/a..b
'

test_expect_success 'cross-check: normalize output matches real git' '
	grit check-ref-format --normalize "/refs//heads//master" >grit_out &&
	$REAL_GIT check-ref-format --normalize "/refs//heads//master" >git_out &&
	test_cmp grit_out git_out
'

test_expect_success 'cross-check: allow-onelevel matches real git' '
	grit check-ref-format --allow-onelevel HEAD &&
	$REAL_GIT check-ref-format --allow-onelevel HEAD
'

test_expect_success 'cross-check: refspec-pattern matches real git' '
	grit check-ref-format --refspec-pattern "refs/heads/*" &&
	$REAL_GIT check-ref-format --refspec-pattern "refs/heads/*"
'

test_expect_success 'cross-check: lock suffix both reject' '
	test_must_fail grit check-ref-format refs/heads/x.lock &&
	test_must_fail $REAL_GIT check-ref-format refs/heads/x.lock
'

test_expect_success 'cross-check: space in name both reject' '
	test_must_fail grit check-ref-format "refs/heads/a b" &&
	test_must_fail $REAL_GIT check-ref-format "refs/heads/a b"
'

test_expect_success 'cross-check: branch mode matches real git' '
	(
	cd repo &&
	grit check-ref-format --branch master >grit_out &&
	$REAL_GIT check-ref-format --branch master >git_out &&
	test_cmp grit_out git_out
	)
'

test_done
