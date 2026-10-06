#!/bin/sh
# Tests for update-ref with symbolic refs, --no-deref, --stdin, -d, -m.

test_description='update-ref with symbolic refs, --no-deref, stdin'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup repository with two commits' '
	(
	grit init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&

	echo "first" >file1.txt &&
	grit add file1.txt &&
	grit commit -m "first commit" &&
	C1=$(grit rev-parse HEAD) &&

	echo "second" >file2.txt &&
	grit add file2.txt &&
	grit commit -m "second commit" &&
	C2=$(grit rev-parse HEAD) &&

	echo "third" >file3.txt &&
	grit add file3.txt &&
	grit commit -m "third commit" &&
	C3=$(grit rev-parse HEAD) &&

	echo $C1 >.c1 &&
	echo $C2 >.c2 &&
	echo $C3 >.c3
	)
'

###########################################################################
# Section 1: Basic update-ref
###########################################################################

test_expect_success 'update-ref creates a new ref' '
	(
	cd repo &&
	C1=$(cat .c1) &&
	grit update-ref refs/heads/new-branch $C1 &&
	grit rev-parse new-branch >actual &&
	test_cmp .c1 actual
	)
'

test_expect_success 'update-ref updates existing ref' '
	(
	cd repo &&
	C2=$(cat .c2) &&
	grit update-ref refs/heads/new-branch $C2 &&
	grit rev-parse new-branch >actual &&
	test_cmp .c2 actual
	)
'

test_expect_success 'update-ref with correct old value succeeds' '
	(
	cd repo &&
	C1=$(cat .c1) && C2=$(cat .c2) &&
	grit update-ref refs/heads/new-branch $C1 $C2 &&
	grit rev-parse new-branch >actual &&
	test_cmp .c1 actual
	)
'

test_expect_success 'update-ref with wrong old value fails' '
	(
	cd repo &&
	C2=$(cat .c2) && C3=$(cat .c3) &&
	test_must_fail grit update-ref refs/heads/new-branch $C2 $C3 2>err &&
	test -s err
	)
'

test_expect_success 'update-ref creates ref in refs/tags namespace' '
	(
	cd repo &&
	C1=$(cat .c1) &&
	grit update-ref refs/tags/test-tag $C1 &&
	grit rev-parse refs/tags/test-tag >actual &&
	test_cmp .c1 actual
	)
'

test_expect_success 'update-ref creates deeply nested ref' '
	(
	cd repo &&
	C2=$(cat .c2) &&
	grit update-ref refs/custom/deep/nested $C2 &&
	grit rev-parse refs/custom/deep/nested >actual &&
	test_cmp .c2 actual
	)
'

###########################################################################
# Section 2: update-ref -d (delete)
###########################################################################

test_expect_success 'update-ref -d deletes a ref' '
	(
	cd repo &&
	C1=$(cat .c1) &&
	grit update-ref refs/heads/to-delete $C1 &&
	grit rev-parse to-delete >actual &&
	test_cmp .c1 actual &&
	grit update-ref -d refs/heads/to-delete &&
	test_must_fail grit rev-parse to-delete 2>err
	)
'

test_expect_success 'update-ref -d with correct old value succeeds' '
	(
	cd repo &&
	C2=$(cat .c2) &&
	grit update-ref refs/heads/to-delete2 $C2 &&
	grit update-ref -d refs/heads/to-delete2 $C2 &&
	test_must_fail grit rev-parse to-delete2 2>err
	)
'

test_expect_success 'update-ref -d nonexistent ref succeeds silently' '
	(
	cd repo &&
	grit update-ref -d refs/heads/does-not-exist
	)
'

###########################################################################
# Section 3: Symbolic refs and update-ref
###########################################################################

test_expect_success 'create symbolic ref with symbolic-ref' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/sym refs/heads/master &&
	grit symbolic-ref refs/heads/sym >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update-ref through symbolic ref updates target' '
	(
	cd repo &&
	C1=$(cat .c1) &&
	ORIG=$(grit rev-parse master) &&
	grit update-ref refs/heads/sym $C1 &&
	grit rev-parse master >actual &&
	test_cmp .c1 actual &&
	grit update-ref refs/heads/master $ORIG
	)
'

test_expect_success 'symbolic ref still points to master after update' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/sym >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update-ref --no-deref replaces symbolic ref with regular' '
	(
	cd repo &&
	C2=$(cat .c2) &&
	grit symbolic-ref refs/heads/sym2 refs/heads/master &&
	grit update-ref --no-deref refs/heads/sym2 $C2 &&
	grit rev-parse refs/heads/sym2 >actual &&
	test_cmp .c2 actual &&
	MASTER_OID=$(grit rev-parse master) &&
	test "$MASTER_OID" != "$(cat .c2)"
	)
'

test_expect_success 'HEAD is a symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update-ref HEAD updates master (deref by default)' '
	(
	cd repo &&
	C1=$(cat .c1) &&
	ORIG=$(grit rev-parse master) &&
	grit update-ref HEAD $C1 &&
	grit rev-parse master >actual &&
	test_cmp .c1 actual &&
	grit update-ref HEAD $ORIG
	)
'

test_expect_success 'update-ref --no-deref HEAD overwrites HEAD itself' '
	(
	cd repo &&
	C2=$(cat .c2) &&
	ORIG_HEAD=$(cat .git/HEAD) &&
	grit update-ref --no-deref HEAD $C2 &&
	HEAD_CONTENT=$(cat .git/HEAD) &&
	test "$HEAD_CONTENT" = "$(cat .c2)" &&
	echo "$ORIG_HEAD" >.git/HEAD
	)
'

###########################################################################
# Section 4: --stdin mode
###########################################################################

test_expect_success 'stdin create creates a ref' '
	(
	cd repo &&
	C1=$(cat .c1) &&
	echo "create refs/heads/stdin1 $C1" | grit update-ref --stdin &&
	grit rev-parse stdin1 >actual &&
	test_cmp .c1 actual
	)
'

test_expect_success 'stdin update with old value' '
	(
	cd repo &&
	C1=$(cat .c1) && C2=$(cat .c2) &&
	echo "update refs/heads/stdin1 $C2 $C1" | grit update-ref --stdin &&
	grit rev-parse stdin1 >actual &&
	test_cmp .c2 actual
	)
'

test_expect_success 'stdin delete removes ref' '
	(
	cd repo &&
	C2=$(cat .c2) &&
	echo "delete refs/heads/stdin1 $C2" | grit update-ref --stdin &&
	test_must_fail grit rev-parse stdin1 2>err
	)
'

test_expect_success 'stdin with multiple commands' '
	(
	cd repo &&
	C1=$(cat .c1) && C2=$(cat .c2) && C3=$(cat .c3) &&
	printf "create refs/heads/multi1 %s\ncreate refs/heads/multi2 %s\ncreate refs/heads/multi3 %s\n" $C1 $C2 $C3 |
	grit update-ref --stdin &&
	grit rev-parse multi1 >actual1 && test_cmp .c1 actual1 &&
	grit rev-parse multi2 >actual2 && test_cmp .c2 actual2 &&
	grit rev-parse multi3 >actual3 && test_cmp .c3 actual3
	)
'

test_expect_success 'stdin verify with correct value succeeds' '
	(
	cd repo &&
	C1=$(cat .c1) &&
	echo "verify refs/heads/multi1 $C1" | grit update-ref --stdin
	)
'

test_expect_success 'stdin verify with wrong value fails' '
	(
	cd repo &&
	C3=$(cat .c3) &&
	echo "verify refs/heads/multi1 $C3" | test_must_fail grit update-ref --stdin 2>err
	)
'

test_expect_success 'stdin cleanup delete multiple' '
	(
	cd repo &&
	C1=$(cat .c1) && C2=$(cat .c2) && C3=$(cat .c3) &&
	printf "delete refs/heads/multi1 %s\ndelete refs/heads/multi2 %s\ndelete refs/heads/multi3 %s\n" $C1 $C2 $C3 |
	grit update-ref --stdin &&
	test_must_fail grit rev-parse multi1 2>err &&
	test_must_fail grit rev-parse multi2 2>err &&
	test_must_fail grit rev-parse multi3 2>err
	)
'

###########################################################################
# Section 5: -m (reflog message)
###########################################################################

test_expect_success 'update-ref -m sets reflog message' '
	(
	cd repo &&
	C1=$(cat .c1) &&
	grit update-ref -m "custom message" refs/heads/with-msg $C1 &&
	grit rev-parse with-msg >actual &&
	test_cmp .c1 actual
	)
'

test_expect_success 'update-ref -m with update' '
	(
	cd repo &&
	C2=$(cat .c2) &&
	grit update-ref -m "update message" refs/heads/with-msg $C2 &&
	grit rev-parse with-msg >actual &&
	test_cmp .c2 actual
	)
'

###########################################################################
# Section 6: Edge cases
###########################################################################

test_expect_success 'update-ref with zero OID should delete ref' '
	(
	cd repo &&
	C1=$(cat .c1) &&
	grit update-ref refs/heads/zero-test $C1 &&
	grit update-ref refs/heads/zero-test 0000000000000000000000000000000000000000 &&
	test_must_fail grit rev-parse zero-test 2>err
	)
'

test_expect_success 'update-ref refuses invalid SHA' '
	(
	cd repo &&
	test_must_fail grit update-ref refs/heads/bad-ref invalidsha 2>err &&
	test -s err
	)
'

test_done
