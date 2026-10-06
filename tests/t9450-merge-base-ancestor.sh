#!/bin/sh
#
# Tests for 'grit merge-base --is-ancestor' — checking ancestor relationships
# in linear, forked, and complex graphs.

test_description='grit merge-base --is-ancestor'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

# ---------------------------------------------------------------------------
# Setup: forked graph
#
#   A --- B --- C --- D  (master)
#          \
#           +-- E --- F  (side)
#                \
#                 +-- G  (topic)
# ---------------------------------------------------------------------------
test_expect_success 'setup: forked history with multiple branches' '
	(
	$REAL_GIT init --initial-branch=master repo &&
	cd repo &&
	$REAL_GIT config user.name "Test User" &&
	$REAL_GIT config user.email "test@example.com" &&
	echo a >file &&
	$REAL_GIT add file &&
	test_tick &&
	$REAL_GIT commit -m A &&
	$REAL_GIT tag A &&
	echo b >file &&
	$REAL_GIT add file &&
	test_tick &&
	$REAL_GIT commit -m B &&
	$REAL_GIT tag B &&
	echo c >file &&
	$REAL_GIT add file &&
	test_tick &&
	$REAL_GIT commit -m C &&
	$REAL_GIT tag C &&
	echo d >file &&
	$REAL_GIT add file &&
	test_tick &&
	$REAL_GIT commit -m D &&
	$REAL_GIT tag D &&
	$REAL_GIT checkout -b side B &&
	echo e >file2 &&
	$REAL_GIT add file2 &&
	test_tick &&
	$REAL_GIT commit -m E &&
	$REAL_GIT tag E &&
	echo f >file2 &&
	$REAL_GIT add file2 &&
	test_tick &&
	$REAL_GIT commit -m F &&
	$REAL_GIT tag F &&
	$REAL_GIT checkout -b topic E &&
	echo g >file3 &&
	$REAL_GIT add file3 &&
	test_tick &&
	$REAL_GIT commit -m G &&
	$REAL_GIT tag G &&
	$REAL_GIT checkout master
	)
'

# ---------------------------------------------------------------------------
# --is-ancestor: true cases (direct lineage)
# ---------------------------------------------------------------------------
test_expect_success 'A is ancestor of B' '
	(
	cd repo &&
	grit merge-base --is-ancestor A B
	)
'

test_expect_success 'A is ancestor of D (transitive)' '
	(
	cd repo &&
	grit merge-base --is-ancestor A D
	)
'

test_expect_success 'B is ancestor of C' '
	(
	cd repo &&
	grit merge-base --is-ancestor B C
	)
'

test_expect_success 'C is ancestor of D' '
	(
	cd repo &&
	grit merge-base --is-ancestor C D
	)
'

test_expect_success 'B is ancestor of F (through side branch)' '
	(
	cd repo &&
	grit merge-base --is-ancestor B F
	)
'

test_expect_success 'A is ancestor of G (transitive through side and topic)' '
	(
	cd repo &&
	grit merge-base --is-ancestor A G
	)
'

test_expect_success 'E is ancestor of G' '
	(
	cd repo &&
	grit merge-base --is-ancestor E G
	)
'

test_expect_success 'E is ancestor of F' '
	(
	cd repo &&
	grit merge-base --is-ancestor E F
	)
'

test_expect_success 'a commit is ancestor of itself' '
	(
	cd repo &&
	grit merge-base --is-ancestor A A
	)
'

test_expect_success 'HEAD~3 is ancestor of HEAD' '
	(
	cd repo &&
	$REAL_GIT checkout master &&
	grit merge-base --is-ancestor HEAD~3 HEAD
	)
'

# ---------------------------------------------------------------------------
# --is-ancestor: false cases
# ---------------------------------------------------------------------------
test_expect_success 'D is NOT ancestor of A' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor D A
	)
'

test_expect_success 'C is NOT ancestor of B' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor C B
	)
'

test_expect_success 'F is NOT ancestor of D (different branch)' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor F D
	)
'

test_expect_success 'D is NOT ancestor of F (different branch)' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor D F
	)
'

test_expect_success 'G is NOT ancestor of F (sibling branches)' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor G F
	)
'

test_expect_success 'F is NOT ancestor of G (sibling branches)' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor F G
	)
'

test_expect_success 'G is NOT ancestor of D (unrelated lines)' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor G D
	)
'

# ---------------------------------------------------------------------------
# --is-ancestor with SHA and ref names
# ---------------------------------------------------------------------------
test_expect_success '--is-ancestor works with full SHA' '
	(
	cd repo &&
	sha_a=$($REAL_GIT rev-parse A) &&
	sha_d=$($REAL_GIT rev-parse D) &&
	grit merge-base --is-ancestor "$sha_a" "$sha_d"
	)
'

test_expect_success '--is-ancestor works with abbreviated SHA' '
	(
	cd repo &&
	short_a=$($REAL_GIT rev-parse --short A) &&
	short_d=$($REAL_GIT rev-parse --short D) &&
	grit merge-base --is-ancestor "$short_a" "$short_d"
	)
'

test_expect_success '--is-ancestor with branch names: A is ancestor of master' '
	(
	cd repo &&
	grit merge-base --is-ancestor A master
	)
'

test_expect_success '--is-ancestor side not ancestor of master (diverged)' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor side master
	)
'

test_expect_success '--is-ancestor master not ancestor of side (diverged)' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor master side
	)
'

# ---------------------------------------------------------------------------
# Compare with real git
# ---------------------------------------------------------------------------
test_expect_success '--is-ancestor A D matches real git exit code' '
	(
	cd repo &&
	grit merge-base --is-ancestor A D &&
	$REAL_GIT merge-base --is-ancestor A D
	)
'

test_expect_success '--is-ancestor D A both fail in grit and real git' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor D A &&
	test_must_fail $REAL_GIT merge-base --is-ancestor D A
	)
'

test_expect_success '--is-ancestor B F matches real git' '
	(
	cd repo &&
	grit merge-base --is-ancestor B F &&
	$REAL_GIT merge-base --is-ancestor B F
	)
'

test_expect_success '--is-ancestor F B both fail' '
	(
	cd repo &&
	test_must_fail grit merge-base --is-ancestor F B &&
	test_must_fail $REAL_GIT merge-base --is-ancestor F B
	)
'

# ---------------------------------------------------------------------------
# merge-base basic (find ancestor) combined with --is-ancestor
# ---------------------------------------------------------------------------
test_expect_success 'merge-base of D and F is B' '
	(
	cd repo &&
	grit merge-base D F >actual &&
	$REAL_GIT rev-parse B >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'merge-base of D and G is B' '
	(
	cd repo &&
	grit merge-base D G >actual &&
	$REAL_GIT rev-parse B >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'merge-base of F and G is E' '
	(
	cd repo &&
	grit merge-base F G >actual &&
	$REAL_GIT rev-parse E >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'merge-base of master and side matches real git' '
	(
	cd repo &&
	grit merge-base master side >actual &&
	$REAL_GIT merge-base master side >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'merge-base of master and topic matches real git' '
	(
	cd repo &&
	grit merge-base master topic >actual &&
	$REAL_GIT merge-base master topic >expect &&
	test_cmp expect actual
	)
'

test_done
