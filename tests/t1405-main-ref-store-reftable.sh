#!/bin/sh
# Test ref store operations: update-ref, symbolic-ref, show-ref combos.

test_description='grit ref store operations (update-ref, symbolic-ref, show-ref)'

cd "$(dirname "$0")" || exit 1

GIT_TEST_DEFAULT_REF_FORMAT=reftable
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_REF_FORMAT GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

# This local regression test re-enters the same repository in each block. Ask
# the harness to reset cwd to the trash root around top-level test bodies.
TEST_OUTPUT_DIRECTORY_OVERRIDE=${TEST_OUTPUT_DIRECTORY_OVERRIDE:-$(pwd)}
export TEST_OUTPUT_DIRECTORY_OVERRIDE

. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repo with commits' '
	grit init ref-repo &&
	cd ref-repo &&
	grit config user.email "test@test.com" &&
	grit config user.name "Test" &&
	echo "first" >file.txt &&
	grit add file.txt &&
	grit commit -m "first" &&
	echo "second" >file.txt &&
	grit add file.txt &&
	grit commit -m "second" &&
	echo "third" >file.txt &&
	grit add file.txt &&
	grit commit -m "third"
'

###########################################################################
# Section 2: update-ref basics
###########################################################################

test_expect_success 'update-ref creates a new ref' '
	cd ref-repo &&
	grit update-ref refs/heads/new-branch HEAD &&
	grit show-ref refs/heads/new-branch >out &&
	test_line_count = 1 out
'

test_expect_success 'update-ref can point to an older commit' '
	cd ref-repo &&
	FIRST=$(grit rev-list --reverse HEAD | head -1) &&
	grit update-ref refs/heads/old-branch "$FIRST" &&
	grit show-ref refs/heads/old-branch >out &&
	grep "$FIRST" out
'

test_expect_success 'update-ref -d deletes a ref' '
	cd ref-repo &&
	grit update-ref refs/heads/to-delete HEAD &&
	grit show-ref refs/heads/to-delete >out &&
	test_line_count = 1 out &&
	grit update-ref -d refs/heads/to-delete &&
	test_must_fail grit show-ref --verify refs/heads/to-delete
'

test_expect_success 'update-ref can create refs in custom namespaces' '
	cd ref-repo &&
	grit update-ref refs/custom/my-ref HEAD &&
	grit show-ref refs/custom/my-ref >out &&
	test_line_count = 1 out
'

test_expect_success 'update-ref --no-deref updates ref directly' '
	cd ref-repo &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	FIRST=$(grit rev-list --reverse HEAD | head -1) &&
	grit update-ref --no-deref refs/heads/noderef-test "$HEAD_OID" &&
	grit update-ref --no-deref refs/heads/noderef-test "$FIRST" "$HEAD_OID" &&
	grit show-ref refs/heads/noderef-test >out &&
	grep "$FIRST" out
'

test_expect_success 'update-ref fails with wrong old value' '
	cd ref-repo &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	FIRST=$(grit rev-list --reverse HEAD | head -1) &&
	grit update-ref refs/heads/cas-test "$HEAD_OID" &&
	test_must_fail grit update-ref refs/heads/cas-test "$FIRST" "$FIRST"
'

test_expect_success 'update-ref --stdin can create refs' '
	cd ref-repo &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	echo "create refs/heads/stdin-branch $HEAD_OID" | grit update-ref --stdin &&
	grit show-ref --verify refs/heads/stdin-branch >out &&
	grep "$HEAD_OID" out
'

test_expect_success 'update-ref --stdin can delete refs' '
	cd ref-repo &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	echo "delete refs/heads/stdin-branch $HEAD_OID" | grit update-ref --stdin &&
	test_must_fail grit show-ref --verify refs/heads/stdin-branch
'

test_expect_success 'update-ref --stdin handles multiple operations' '
	cd ref-repo &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	FIRST=$(grit rev-list --reverse HEAD | head -1) &&
	printf "create refs/heads/multi-a %s\ncreate refs/heads/multi-b %s\n" "$HEAD_OID" "$FIRST" |
		grit update-ref --stdin &&
	grit show-ref refs/heads/multi-a >out_a &&
	grit show-ref refs/heads/multi-b >out_b &&
	grep "$HEAD_OID" out_a &&
	grep "$FIRST" out_b
'

###########################################################################
# Section 3: symbolic-ref
###########################################################################

test_expect_success 'symbolic-ref reads HEAD' '
	cd ref-repo &&
	grit symbolic-ref HEAD >out &&
	grep "refs/heads/master" out
'

test_expect_success 'symbolic-ref sets HEAD to another branch' '
	cd ref-repo &&
	grit symbolic-ref HEAD refs/heads/new-branch &&
	grit symbolic-ref HEAD >out &&
	grep "refs/heads/new-branch" out &&
	grit symbolic-ref HEAD refs/heads/master
'

test_expect_success 'symbolic-ref creates arbitrary symbolic ref' '
	cd ref-repo &&
	grit symbolic-ref refs/symref/test refs/heads/master &&
	grit symbolic-ref refs/symref/test >out &&
	grep "refs/heads/master" out
'

test_expect_success 'symbolic-ref -d deletes a symbolic ref' '
	cd ref-repo &&
	grit symbolic-ref refs/symref/deleteme refs/heads/master &&
	grit symbolic-ref -d refs/symref/deleteme &&
	test_must_fail grit symbolic-ref refs/symref/deleteme
'

test_expect_success 'symbolic-ref -d refuses to delete HEAD' '
	cd ref-repo &&
	test_must_fail grit symbolic-ref -d HEAD
'

test_expect_success 'symbolic-ref fails on non-symbolic ref' '
	cd ref-repo &&
	test_must_fail grit symbolic-ref refs/heads/master
'

###########################################################################
# Section 4: show-ref
###########################################################################

test_expect_success 'show-ref lists all refs' '
	cd ref-repo &&
	grit show-ref >out &&
	grep "refs/heads/master" out &&
	grep "refs/heads/new-branch" out
'

test_expect_success 'show-ref --heads lists only heads' '
	cd ref-repo &&
	grit tag test-tag &&
	grit show-ref --heads >out &&
	grep "refs/heads/" out &&
	! grep "refs/tags/" out
'

test_expect_success 'show-ref --tags lists only tags' '
	cd ref-repo &&
	grit show-ref --tags >out &&
	grep "refs/tags/" out &&
	! grep "refs/heads/" out
'

test_expect_success 'show-ref with pattern filters refs' '
	cd ref-repo &&
	grit show-ref refs/heads/master >out &&
	test_line_count = 1 out &&
	grep "refs/heads/master" out
'

test_expect_success 'show-ref --verify checks exact ref' '
	cd ref-repo &&
	grit show-ref --verify refs/heads/master >out &&
	test_line_count = 1 out
'

test_expect_success 'show-ref --verify fails for nonexistent ref' '
	cd ref-repo &&
	test_must_fail grit show-ref --verify refs/heads/nonexistent
'

test_expect_success 'show-ref --quiet suppresses output' '
	cd ref-repo &&
	grit show-ref --quiet refs/heads/master >out &&
	test_line_count = 0 out
'

test_expect_success 'show-ref --quiet returns error for missing ref' '
	cd ref-repo &&
	test_must_fail grit show-ref --quiet refs/heads/nonexistent
'

test_expect_success 'show-ref --hash shows only hashes' '
	cd ref-repo &&
	grit show-ref --hash refs/heads/master >out &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	grep "$HEAD_OID" out &&
	! grep "refs/" out
'

test_expect_success 'show-ref -d dereferences tags' '
	cd ref-repo &&
	grit show-ref -d >out &&
	grep "refs/tags/test-tag" out
'

###########################################################################
# Section 5: combined operations
###########################################################################

test_expect_success 'create branch via update-ref, verify with show-ref' '
	cd ref-repo &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	grit update-ref refs/heads/verify-combo "$HEAD_OID" &&
	grit show-ref --verify refs/heads/verify-combo >out &&
	grep "$HEAD_OID" out
'

test_expect_success 'symbolic-ref + show-ref interaction' '
	cd ref-repo &&
	grit symbolic-ref HEAD refs/heads/master &&
	HEAD_TARGET=$(grit symbolic-ref HEAD) &&
	grit show-ref "$HEAD_TARGET" >out &&
	test_line_count = 1 out
'

test_expect_success 'update-ref to different commits, show-ref verifies' '
	cd ref-repo &&
	FIRST=$(grit rev-list --reverse HEAD | head -1) &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	grit update-ref refs/heads/flip "$FIRST" &&
	grit show-ref --hash refs/heads/flip >out1 &&
	grep "$FIRST" out1 &&
	grit update-ref refs/heads/flip "$HEAD_OID" "$FIRST" &&
	grit show-ref --hash refs/heads/flip >out2 &&
	grep "$HEAD_OID" out2
'

test_done
