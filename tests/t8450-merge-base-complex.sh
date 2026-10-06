#!/bin/sh
# Tests for merge-base with complex DAGs, octopus merges, --is-ancestor chains.

test_description='merge-base complex DAG scenarios'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

M=1200000000
Z=+0000

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# Helper: create a tagged commit.
# Usage: mk OFFSET NAME [PARENT_TAG ...]
mk () {
	OFFSET=$1 && NAME=$2 && shift 2 &&
	PARENTS= &&
	for P; do
		PARENTS="${PARENTS}-p $(git rev-parse $P) "
	done &&
	GIT_COMMITTER_DATE="$(($M + $OFFSET)) $Z" &&
	GIT_AUTHOR_DATE=$GIT_COMMITTER_DATE &&
	export GIT_COMMITTER_DATE GIT_AUTHOR_DATE &&
	commit=$(echo "$NAME" | git commit-tree "$(git write-tree)" $PARENTS) &&
	git update-ref "refs/tags/$NAME" "$commit"
}

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo
	)
'

# Build a diamond DAG:
#   A
#  / \
# B   C
#  \ /
#   D (merge of B and C)
test_expect_success 'setup diamond DAG' '
	(
	cd repo &&
	mk 1 DA &&
	mk 2 DB DA &&
	mk 3 DC DA &&
	mk 4 DD DB DC
	)
'

test_expect_success 'merge-base of diamond sides is the root' '
	(
	cd repo &&
	git rev-parse DA >expect &&
	git merge-base DB DC >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'merge-base of merge and root is root' '
	(
	cd repo &&
	git rev-parse DA >expect &&
	git merge-base DD DA >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'merge-base of merge and left parent is left parent' '
	(
	cd repo &&
	git rev-parse DB >expect &&
	git merge-base DD DB >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'merge-base of merge and right parent is right parent' '
	(
	cd repo &&
	git rev-parse DC >expect &&
	git merge-base DD DC >actual &&
	test_cmp expect actual
	)
'

test_expect_success '--is-ancestor: root is ancestor of merge' '
	(
	cd repo &&
	git merge-base --is-ancestor DA DD
	)
'

test_expect_success '--is-ancestor: left parent is ancestor of merge' '
	(
	cd repo &&
	git merge-base --is-ancestor DB DD
	)
'

test_expect_success '--is-ancestor: right parent is ancestor of merge' '
	(
	cd repo &&
	git merge-base --is-ancestor DC DD
	)
'

test_expect_success '--is-ancestor: merge is NOT ancestor of root' '
	(
	cd repo &&
	test_must_fail git merge-base --is-ancestor DD DA
	)
'

test_expect_success '--is-ancestor: commit is its own ancestor' '
	(
	cd repo &&
	git merge-base --is-ancestor DA DA
	)
'

# Extend: DA -> DB -> DE -> DF  and  DA -> DC -> DG
test_expect_success 'setup extended branches' '
	(
	cd repo &&
	mk 5 DE DB &&
	mk 6 DF DE &&
	mk 7 DG DC
	)
'

test_expect_success 'merge-base of divergent branch tips is common root' '
	(
	cd repo &&
	git rev-parse DA >expect &&
	git merge-base DF DG >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'merge-base of extended branch and merge is left parent' '
	(
	cd repo &&
	git rev-parse DB >expect &&
	git merge-base DF DD >actual &&
	test_cmp expect actual
	)
'

test_expect_success '--is-ancestor: root is ancestor of deep descendant' '
	(
	cd repo &&
	git merge-base --is-ancestor DA DF
	)
'

test_expect_success '--is-ancestor: mid-chain is ancestor of tip' '
	(
	cd repo &&
	git merge-base --is-ancestor DE DF
	)
'

test_expect_success '--is-ancestor: tip is NOT ancestor of mid-chain' '
	(
	cd repo &&
	test_must_fail git merge-base --is-ancestor DF DE
	)
'

test_expect_success '--is-ancestor: parallel branches are not ancestors' '
	(
	cd repo &&
	test_must_fail git merge-base --is-ancestor DG DF
	)
'

# Octopus merge: DH = merge(DD, DF, DG)
test_expect_success 'setup octopus merge' '
	(
	cd repo &&
	mk 8 DH DD DF DG
	)
'

test_expect_success 'merge-base of two octopus parents' '
	(
	cd repo &&
	git rev-parse DB >expect &&
	git merge-base DD DF >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'merge-base of other octopus parents' '
	(
	cd repo &&
	git rev-parse DC >expect &&
	git merge-base DD DG >actual &&
	test_cmp expect actual
	)
'

test_expect_success '--is-ancestor: all ancestors reachable through octopus' '
	(
	cd repo &&
	git merge-base --is-ancestor DA DH &&
	git merge-base --is-ancestor DB DH &&
	git merge-base --is-ancestor DC DH &&
	git merge-base --is-ancestor DD DH &&
	git merge-base --is-ancestor DE DH &&
	git merge-base --is-ancestor DF DH &&
	git merge-base --is-ancestor DG DH
	)
'

test_expect_success '--is-ancestor: octopus is NOT ancestor of any parent' '
	(
	cd repo &&
	test_must_fail git merge-base --is-ancestor DH DD &&
	test_must_fail git merge-base --is-ancestor DH DF &&
	test_must_fail git merge-base --is-ancestor DH DG
	)
'

test_expect_success '--independent with octopus and its parents' '
	(
	cd repo &&
	result=$(git merge-base --independent DH DD DF DG) &&
	echo "$result" >actual &&
	git rev-parse DH >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--independent with two unrelated branches' '
	(
	cd repo &&
	result=$(git merge-base --independent DF DG) &&
	printf "%s\n" $result | sort >actual &&
	printf "%s\n" "$(git rev-parse DF)" "$(git rev-parse DG)" | sort >expect &&
	test_cmp expect actual
	)
'

# Criss-cross merge:
#   X0
#  /  \
# X1   X2
# |\ /|
# | X  |
# |/ \|
# X3  X4
test_expect_success 'setup criss-cross merge' '
	(
	cd repo &&
	mk 10 X0 &&
	mk 11 X1 X0 &&
	mk 12 X2 X0 &&
	mk 13 X3 X1 X2 &&
	mk 14 X4 X2 X1
	)
'

test_expect_success 'merge-base --all of criss-cross returns both bases' '
	(
	cd repo &&
	git merge-base --all X3 X4 >actual &&
	sort actual >actual_sorted &&
	printf "%s\n" "$(git rev-parse X1)" "$(git rev-parse X2)" | sort >expect &&
	test_cmp expect actual_sorted
	)
'

test_expect_success 'merge-base of criss-cross returns one valid base' '
	(
	cd repo &&
	mb=$(git merge-base X3 X4) &&
	x1=$(git rev-parse X1) &&
	x2=$(git rev-parse X2) &&
	test "$mb" = "$x1" || test "$mb" = "$x2"
	)
'

test_expect_success '--is-ancestor: root is ancestor through criss-cross' '
	(
	cd repo &&
	git merge-base --is-ancestor X0 X3 &&
	git merge-base --is-ancestor X0 X4
	)
'

test_expect_success '--is-ancestor: X1 is ancestor of X4 (criss-cross path)' '
	(
	cd repo &&
	git merge-base --is-ancestor X1 X4
	)
'

# Long linear chain
test_expect_success 'setup long linear chain' '
	(
	cd repo &&
	mk 20 L0 &&
	mk 21 L1 L0 &&
	mk 22 L2 L1 &&
	mk 23 L3 L2 &&
	mk 24 L4 L3 &&
	mk 25 L5 L4 &&
	mk 26 L6 L5 &&
	mk 27 L7 L6 &&
	mk 28 L8 L7 &&
	mk 29 L9 L8 &&
	mk 30 L10 L9
	)
'

test_expect_success 'merge-base of linear chain endpoints is earlier commit' '
	(
	cd repo &&
	git rev-parse L0 >expect &&
	git merge-base L0 L10 >actual &&
	test_cmp expect actual
	)
'

test_expect_success '--is-ancestor along linear chain' '
	(
	cd repo &&
	git merge-base --is-ancestor L0 L10 &&
	git merge-base --is-ancestor L5 L10 &&
	test_must_fail git merge-base --is-ancestor L10 L0
	)
'

test_expect_success 'merge-base of same commit returns itself' '
	(
	cd repo &&
	git rev-parse L5 >expect &&
	git merge-base L5 L5 >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'merge-base with tag refs works' '
	(
	cd repo &&
	git rev-parse DA >expect &&
	git merge-base refs/tags/DB refs/tags/DC >actual &&
	test_cmp expect actual
	)
'

test_done
