#!/bin/sh
# Tests for grit symbolic-ref: read, write, delete, --short,
# --no-recurse, -q, and -m options.

test_description='grit symbolic-ref HEAD management and options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with branches' '
	(
	grit init --initial-branch=master repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo one >one.txt &&
	grit add . &&
	grit commit -m "first" &&
	grit branch feature &&
	grit branch develop &&
	grit branch release/1.0
	)
'

###########################################################################
# Section 2: Read symbolic-ref
###########################################################################

test_expect_success 'symbolic-ref HEAD shows current branch' '
	(
	cd repo &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/master" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'symbolic-ref HEAD output is a single line' '
	(
	cd repo &&
	grit symbolic-ref HEAD >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'symbolic-ref HEAD output starts with refs/' '
	(
	cd repo &&
	grit symbolic-ref HEAD >actual &&
	grep "^refs/" actual
	)
'

###########################################################################
# Section 3: --short
###########################################################################

test_expect_success 'symbolic-ref --short HEAD shows short branch name' '
	(
	cd repo &&
	grit symbolic-ref --short HEAD >actual &&
	echo "master" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'symbolic-ref --short HEAD has no refs/ prefix' '
	(
	cd repo &&
	grit symbolic-ref --short HEAD >actual &&
	! grep "refs/" actual
	)
'

test_expect_success 'symbolic-ref --short HEAD on nested branch' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/release/1.0 &&
	grit symbolic-ref --short HEAD >actual &&
	echo "release/1.0" >expected &&
	test_cmp expected actual &&
	grit symbolic-ref HEAD refs/heads/master
	)
'

###########################################################################
# Section 4: Write symbolic-ref
###########################################################################

test_expect_success 'symbolic-ref sets HEAD to feature branch' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/feature &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/feature" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'symbolic-ref sets HEAD to develop branch' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/develop &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/develop" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'symbolic-ref sets HEAD back to master' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/master &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/master" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'symbolic-ref can set HEAD to release branch' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/release/1.0 &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/release/1.0" >expected &&
	test_cmp expected actual &&
	grit symbolic-ref HEAD refs/heads/master
	)
'

test_expect_success 'symbolic-ref can create custom symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref refs/custom/sym refs/heads/feature &&
	grit symbolic-ref refs/custom/sym >actual &&
	echo "refs/heads/feature" >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 5: Delete symbolic-ref (-d)
###########################################################################

test_expect_success 'symbolic-ref -d deletes a custom symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref refs/custom/del refs/heads/master &&
	grit symbolic-ref refs/custom/del >actual &&
	echo "refs/heads/master" >expected &&
	test_cmp expected actual &&
	grit symbolic-ref -d refs/custom/del &&
	test_must_fail grit symbolic-ref refs/custom/del
	)
'

test_expect_success 'symbolic-ref --delete is same as -d' '
	(
	cd repo &&
	grit symbolic-ref refs/custom/del2 refs/heads/feature &&
	grit symbolic-ref --delete refs/custom/del2 &&
	test_must_fail grit symbolic-ref refs/custom/del2
	)
'

test_expect_success 'symbolic-ref -d on nonexistent ref fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref -d refs/custom/nonexist
	)
'

###########################################################################
# Section 6: -q (quiet)
###########################################################################

test_expect_success 'symbolic-ref -q on valid ref succeeds silently' '
	(
	cd repo &&
	grit symbolic-ref -q HEAD >actual &&
	echo "refs/heads/master" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'symbolic-ref -q on non-symbolic ref fails quietly' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	grit update-ref refs/test/nonsym $HEAD &&
	test_must_fail grit symbolic-ref -q refs/test/nonsym 2>err &&
	test_must_be_empty err
	)
'

###########################################################################
# Section 7: --no-recurse
###########################################################################

test_expect_success 'symbolic-ref --no-recurse stops after one level' '
	(
	cd repo &&
	grit symbolic-ref refs/sym/chain refs/heads/master &&
	grit symbolic-ref --no-recurse refs/sym/chain >actual &&
	echo "refs/heads/master" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'symbolic-ref --no-recurse on HEAD' '
	(
	cd repo &&
	grit symbolic-ref --no-recurse HEAD >actual &&
	echo "refs/heads/master" >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 8: -m (reflog message)
###########################################################################

test_expect_success 'symbolic-ref -m sets HEAD with message' '
	(
	cd repo &&
	grit symbolic-ref -m "switching to feature" HEAD refs/heads/feature &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/feature" >expected &&
	test_cmp expected actual &&
	grit symbolic-ref HEAD refs/heads/master
	)
'

test_expect_success 'symbolic-ref -m does not affect ref target' '
	(
	cd repo &&
	grit symbolic-ref -m "msg" HEAD refs/heads/develop &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/develop" >expected &&
	test_cmp expected actual &&
	grit symbolic-ref HEAD refs/heads/master
	)
'

###########################################################################
# Section 9: Edge cases
###########################################################################

test_expect_success 'symbolic-ref on non-symbolic ref fails' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	grit update-ref refs/test/plain $HEAD &&
	test_must_fail grit symbolic-ref refs/test/plain
	)
'

test_expect_success 'symbolic-ref HEAD unchanged by failed read of other ref' '
	(
	cd repo &&
	grit symbolic-ref HEAD >before &&
	test_must_fail grit symbolic-ref refs/test/plain &&
	grit symbolic-ref HEAD >after &&
	test_cmp before after
	)
'

test_expect_success 'symbolic-ref can round-trip through set and read' '
	(
	cd repo &&
	grit symbolic-ref refs/roundtrip/a refs/heads/feature &&
	grit symbolic-ref refs/roundtrip/a >actual &&
	echo "refs/heads/feature" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'symbolic-ref set then --short then read' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/develop &&
	grit symbolic-ref --short HEAD >actual &&
	echo "develop" >expected &&
	test_cmp expected actual &&
	grit symbolic-ref HEAD refs/heads/master
	)
'

test_expect_success 'symbolic-ref set and delete cycle' '
	(
	cd repo &&
	grit symbolic-ref refs/cycle/ref refs/heads/master &&
	grit symbolic-ref -d refs/cycle/ref &&
	grit symbolic-ref refs/cycle/ref refs/heads/feature &&
	grit symbolic-ref refs/cycle/ref >actual &&
	echo "refs/heads/feature" >expected &&
	test_cmp expected actual &&
	grit symbolic-ref -d refs/cycle/ref
	)
'

###########################################################################
# Section 10: Cross-check with real git
###########################################################################

test_expect_success 'setup cross-check repo' '
	(
	$REAL_GIT init --initial-branch=master cross &&
	cd cross &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo x >x.txt &&
	$REAL_GIT add . &&
	$REAL_GIT commit -m "init" &&
	$REAL_GIT branch br1
	)
'

test_expect_success 'symbolic-ref HEAD matches real git' '
	(
	cd cross &&
	grit symbolic-ref HEAD >grit_out &&
	$REAL_GIT symbolic-ref HEAD >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'symbolic-ref --short HEAD matches real git' '
	(
	cd cross &&
	grit symbolic-ref --short HEAD >grit_out &&
	$REAL_GIT symbolic-ref --short HEAD >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'symbolic-ref set then read matches real git' '
	(
	cd cross &&
	grit symbolic-ref HEAD refs/heads/br1 &&
	$REAL_GIT symbolic-ref HEAD refs/heads/br1 &&
	grit symbolic-ref HEAD >grit_out &&
	$REAL_GIT symbolic-ref HEAD >git_out &&
	test_cmp grit_out git_out &&
	grit symbolic-ref HEAD refs/heads/master &&
	$REAL_GIT symbolic-ref HEAD refs/heads/master
	)
'

test_expect_success 'symbolic-ref --short after set matches real git' '
	(
	cd cross &&
	grit symbolic-ref HEAD refs/heads/br1 &&
	$REAL_GIT symbolic-ref HEAD refs/heads/br1 &&
	grit symbolic-ref --short HEAD >grit_out &&
	$REAL_GIT symbolic-ref --short HEAD >git_out &&
	test_cmp grit_out git_out &&
	grit symbolic-ref HEAD refs/heads/master &&
	$REAL_GIT symbolic-ref HEAD refs/heads/master
	)
'

test_done
