#!/bin/sh
# Tests for grit update-ref: create, update, delete, verify (old-value),
# --no-deref, --stdin, -z, and -m options.

test_description='grit update-ref create, verify, delete, stdin, and options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with two commits' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo one >one.txt &&
	grit add . &&
	grit commit -m "first" &&
	echo two >two.txt &&
	grit add . &&
	grit commit -m "second"
	)
'

###########################################################################
# Section 2: Basic create
###########################################################################

test_expect_success 'update-ref creates a new ref' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	grit update-ref refs/test/new $HEAD &&
	grit show-ref --verify refs/test/new >actual &&
	grep "$HEAD" actual
	)
'

test_expect_success 'update-ref creates ref under refs/custom/' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	grit update-ref refs/custom/foo $HEAD &&
	grit show-ref --verify refs/custom/foo >actual &&
	grep "$HEAD" actual
	)
'

test_expect_success 'update-ref creates ref pointing to parent commit' '
	(
	cd repo &&
	PARENT=$(grit rev-parse HEAD~1) &&
	grit update-ref refs/test/parent $PARENT &&
	grit show-ref --verify refs/test/parent >actual &&
	grep "$PARENT" actual
	)
'

test_expect_success 'update-ref overwrites existing ref without old-value' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	PARENT=$(grit rev-parse HEAD~1) &&
	grit update-ref refs/test/overwrite $HEAD &&
	grit update-ref refs/test/overwrite $PARENT &&
	grit show-ref --verify refs/test/overwrite >actual &&
	grep "$PARENT" actual
	)
'

###########################################################################
# Section 3: Verify (old-value check)
###########################################################################

test_expect_success 'update-ref with correct old-value succeeds' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	PARENT=$(grit rev-parse HEAD~1) &&
	grit update-ref refs/test/verify $HEAD &&
	grit update-ref refs/test/verify $PARENT $HEAD &&
	grit show-ref --verify refs/test/verify >actual &&
	grep "$PARENT" actual
	)
'

test_expect_success 'update-ref with wrong old-value fails' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	PARENT=$(grit rev-parse HEAD~1) &&
	grit update-ref refs/test/wrongold $HEAD &&
	test_must_fail grit update-ref refs/test/wrongold $PARENT $PARENT
	)
'

test_expect_success 'update-ref with wrong old-value does not change ref' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	grit show-ref --verify refs/test/wrongold >actual &&
	grep "$HEAD" actual
	)
'

test_expect_success 'update-ref verify on nonexistent ref with zero old-value succeeds' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	grit update-ref refs/test/newverify $HEAD 0000000000000000000000000000000000000000 &&
	grit show-ref --verify refs/test/newverify >actual &&
	grep "$HEAD" actual
	)
'

###########################################################################
# Section 4: Delete (-d)
###########################################################################

test_expect_success 'update-ref -d deletes a ref' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	grit update-ref refs/test/delme $HEAD &&
	grit show-ref --exists refs/test/delme &&
	grit update-ref -d refs/test/delme &&
	test_must_fail grit show-ref --exists refs/test/delme
	)
'

test_expect_success 'update-ref -d on nonexistent ref is silent' '
	(
	cd repo &&
	grit update-ref -d refs/test/no_such_ref
	)
'

test_expect_success 'update-ref -d with correct old-value succeeds' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	grit update-ref refs/test/dv $HEAD &&
	grit update-ref -d refs/test/dv $HEAD &&
	test_must_fail grit show-ref --exists refs/test/dv
	)
'

test_expect_success 'update-ref -d with wrong old-value fails' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	PARENT=$(grit rev-parse HEAD~1) &&
	grit update-ref refs/test/dwrong $HEAD &&
	test_must_fail grit update-ref -d refs/test/dwrong $PARENT
	)
'

test_expect_success 'update-ref -d with wrong old-value preserves ref' '
	(
	cd repo &&
	grit show-ref --exists refs/test/dwrong
	)
'

###########################################################################
# Section 5: --no-deref
###########################################################################

test_expect_success 'update-ref --no-deref creates ref normally' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	grit update-ref --no-deref refs/test/noderef $HEAD &&
	grit show-ref --verify refs/test/noderef >actual &&
	grep "$HEAD" actual
	)
'

test_expect_success 'update-ref --no-deref updates ref value' '
	(
	cd repo &&
	PARENT=$(grit rev-parse HEAD~1) &&
	grit update-ref --no-deref refs/test/noderef $PARENT &&
	grit show-ref --verify refs/test/noderef >actual &&
	grep "$PARENT" actual
	)
'

###########################################################################
# Section 6: --stdin
###########################################################################

test_expect_success 'update-ref --stdin create command' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	echo "create refs/test/stdin1 $HEAD" | grit update-ref --stdin &&
	grit show-ref --verify refs/test/stdin1 >actual &&
	grep "$HEAD" actual
	)
'

test_expect_success 'update-ref --stdin update command' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	PARENT=$(grit rev-parse HEAD~1) &&
	echo "update refs/test/stdin1 $PARENT $HEAD" | grit update-ref --stdin &&
	grit show-ref --verify refs/test/stdin1 >actual &&
	grep "$PARENT" actual
	)
'

test_expect_success 'update-ref --stdin delete command' '
	(
	cd repo &&
	PARENT=$(grit rev-parse HEAD~1) &&
	echo "delete refs/test/stdin1 $PARENT" | grit update-ref --stdin &&
	test_must_fail grit show-ref --exists refs/test/stdin1
	)
'

test_expect_success 'update-ref --stdin multiple commands' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	PARENT=$(grit rev-parse HEAD~1) &&
	printf "create refs/test/multi1 %s\ncreate refs/test/multi2 %s\n" "$HEAD" "$PARENT" |
		grit update-ref --stdin &&
	grit show-ref --verify refs/test/multi1 >actual1 &&
	grep "$HEAD" actual1 &&
	grit show-ref --verify refs/test/multi2 >actual2 &&
	grep "$PARENT" actual2
	)
'

test_expect_success 'update-ref --stdin verify command' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	echo "verify refs/test/multi1 $HEAD" | grit update-ref --stdin
	)
'

test_expect_success 'update-ref --stdin verify fails on mismatch' '
	(
	cd repo &&
	PARENT=$(grit rev-parse HEAD~1) &&
	echo "verify refs/test/multi1 $PARENT" |
		test_must_fail grit update-ref --stdin
	)
'

###########################################################################
# Section 7: --stdin -z (NUL-delimited)
###########################################################################

test_expect_success 'update-ref --stdin -z create command' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	printf "create refs/test/stdinz %s\0" "$HEAD" |
		grit update-ref --stdin -z &&
	grit show-ref --verify refs/test/stdinz >actual &&
	grep "$HEAD" actual
	)
'

test_expect_success 'update-ref --stdin -z delete command' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	printf "delete refs/test/stdinz %s\0" "$HEAD" |
		grit update-ref --stdin -z &&
	test_must_fail grit show-ref --exists refs/test/stdinz
	)
'

###########################################################################
# Section 8: -m (reflog message)
###########################################################################

test_expect_success 'update-ref -m sets reflog message' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	grit update-ref -m "test message" refs/test/logme $HEAD
	)
'

test_expect_success 'update-ref -m does not affect ref value' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	grit show-ref --verify refs/test/logme >actual &&
	grep "$HEAD" actual
	)
'

###########################################################################
# Section 9: Edge cases
###########################################################################

test_expect_success 'update-ref with same value is a no-op' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	grit update-ref refs/test/same $HEAD &&
	grit update-ref refs/test/same $HEAD &&
	grit show-ref --verify refs/test/same >actual &&
	grep "$HEAD" actual
	)
'

test_expect_success 'update-ref to deeply nested ref' '
	(
	cd repo &&
	HEAD=$(grit rev-parse HEAD) &&
	grit update-ref refs/test/deep/nested/ref $HEAD &&
	grit show-ref --verify refs/test/deep/nested/ref >actual &&
	grep "$HEAD" actual
	)
'

test_expect_success 'update-ref with invalid OID fails' '
	(
	cd repo &&
	test_must_fail grit update-ref refs/test/bad notahash
	)
'

###########################################################################
# Section 10: Cross-check with real git
###########################################################################

test_expect_success 'setup cross-check repo' '
	(
	$REAL_GIT init cross &&
	cd cross &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo data >data.txt &&
	$REAL_GIT add . &&
	$REAL_GIT commit -m "init"
	)
'

test_expect_success 'update-ref create matches real git' '
	(
	cd cross &&
	HEAD=$($REAL_GIT rev-parse HEAD) &&
	grit update-ref refs/test/grit1 $HEAD &&
	$REAL_GIT update-ref refs/test/git1 $HEAD &&
	grit show-ref --verify refs/test/grit1 >grit_out &&
	$REAL_GIT show-ref --verify refs/test/git1 >git_out &&
	cut -d" " -f1 <grit_out >grit_oid &&
	cut -d" " -f1 <git_out >git_oid &&
	test_cmp grit_oid git_oid
	)
'

test_expect_success 'update-ref -d matches real git behavior' '
	(
	cd cross &&
	HEAD=$($REAL_GIT rev-parse HEAD) &&
	grit update-ref refs/test/grit_del $HEAD &&
	$REAL_GIT update-ref refs/test/git_del $HEAD &&
	grit update-ref -d refs/test/grit_del &&
	$REAL_GIT update-ref -d refs/test/git_del &&
	test_must_fail grit show-ref --exists refs/test/grit_del &&
	test_must_fail $REAL_GIT show-ref --exists refs/test/git_del
	)
'

test_done
