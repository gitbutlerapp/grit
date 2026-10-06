#!/bin/sh
# Tests for for-each-ref filtering: --points-at, --merged, --no-merged,
# --contains, --count, pattern matching, combined filters.

test_description='for-each-ref filter options'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup — branching topology
###########################################################################

test_expect_success 'setup repository with branches and tags' '
	(
	grit init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&

	echo "base" >file.txt &&
	grit add file.txt &&
	grit commit -m "initial" &&
	C1=$(grit rev-parse HEAD) &&

	echo "second" >file2.txt &&
	grit add file2.txt &&
	grit commit -m "second" &&
	C2=$(grit rev-parse HEAD) &&

	echo "third" >file3.txt &&
	grit add file3.txt &&
	grit commit -m "third" &&
	C3=$(grit rev-parse HEAD) &&

	grit branch old $C1 &&
	grit branch mid $C2 &&
	grit branch diverge $C1 &&
	grit tag v1.0 $C1 &&
	grit tag v2.0 $C2 &&
	grit tag v3.0 $C3 &&

	echo $C1 >.c1 &&
	echo $C2 >.c2 &&
	echo $C3 >.c3
	)
'

###########################################################################
# Section 1: Pattern filtering
###########################################################################

test_expect_success 'for-each-ref lists all refs' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" >actual &&
	grep "refs/heads/master" actual &&
	grep "refs/heads/old" actual &&
	grep "refs/tags/v1.0" actual
	)
'

test_expect_success 'for-each-ref refs/heads/ lists only branches' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/ >actual &&
	grep "refs/heads/master" actual &&
	grep "refs/heads/old" actual &&
	! grep "refs/tags" actual
	)
'

test_expect_success 'for-each-ref refs/tags/ lists only tags' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/tags/ >actual &&
	grep "refs/tags/v1.0" actual &&
	grep "refs/tags/v2.0" actual &&
	! grep "refs/heads" actual
	)
'

test_expect_success 'pattern refs/heads/master matches only master' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/master >actual &&
	grep "refs/heads/master" actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'pattern refs/heads/old matches old branch' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/old >actual &&
	grep "refs/heads/old" actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'pattern refs/heads/nonexistent returns nothing' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/nonexistent >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 2: --count
###########################################################################

test_expect_success 'for-each-ref --count=1 returns exactly 1 ref' '
	(
	cd repo &&
	grit for-each-ref --count=1 --format="%(refname)" >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'for-each-ref --count=2 returns exactly 2 refs' '
	(
	cd repo &&
	grit for-each-ref --count=2 --format="%(refname)" >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'for-each-ref --count=100 returns all available' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" >all &&
	grit for-each-ref --count=100 --format="%(refname)" >actual &&
	test_cmp all actual
	)
'

test_expect_success 'for-each-ref --count=1 refs/tags/ limits tag output' '
	(
	cd repo &&
	grit for-each-ref --count=1 --format="%(refname)" refs/tags/ >actual &&
	test_line_count = 1 actual &&
	grep "refs/tags/" actual
	)
'

###########################################################################
# Section 3: --points-at
###########################################################################

test_expect_success 'points-at C1 returns old, diverge, and v1.0' '
	(
	cd repo &&
	C1=$(cat .c1) &&
	grit for-each-ref --points-at $C1 --format="%(refname)" >actual &&
	grep "refs/heads/old" actual &&
	grep "refs/heads/diverge" actual &&
	grep "refs/tags/v1.0" actual
	)
'

test_expect_success 'points-at C2 returns mid and v2.0' '
	(
	cd repo &&
	C2=$(cat .c2) &&
	grit for-each-ref --points-at $C2 --format="%(refname)" >actual &&
	grep "refs/heads/mid" actual &&
	grep "refs/tags/v2.0" actual &&
	! grep "refs/heads/master" actual
	)
'

test_expect_success 'points-at C3 returns master and v3.0' '
	(
	cd repo &&
	C3=$(cat .c3) &&
	grit for-each-ref --points-at $C3 --format="%(refname)" >actual &&
	grep "refs/heads/master" actual &&
	grep "refs/tags/v3.0" actual
	)
'

test_expect_success 'points-at with pattern limits to matching refs' '
	(
	cd repo &&
	C1=$(cat .c1) &&
	grit for-each-ref --points-at $C1 --format="%(refname)" refs/heads/ >actual &&
	grep "refs/heads/old" actual &&
	! grep "refs/tags" actual
	)
'

test_expect_success 'points-at with SHA pointing to no ref returns empty' '
	(
	cd repo &&
	C2=$(cat .c2) &&
	grit branch temp-for-test $C2 &&
	grit branch -d temp-for-test &&
	grit for-each-ref --points-at $C2 --format="%(refname)" refs/heads/temp >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 4: --sort with filters
###########################################################################

test_expect_success 'sort by refname ascending' '
	(
	cd repo &&
	grit for-each-ref --sort=refname --format="%(refname)" refs/heads/ >actual &&
	head -1 actual >first &&
	echo "refs/heads/diverge" >expect &&
	test_cmp expect first
	)
'

test_expect_success 'sort by -refname descending' '
	(
	cd repo &&
	grit for-each-ref --sort=-refname --format="%(refname)" refs/heads/ >actual &&
	head -1 actual >first &&
	echo "refs/heads/old" >expect &&
	test_cmp expect first
	)
'

test_expect_success 'sort by objectname' '
	(
	cd repo &&
	grit for-each-ref --sort=objectname --format="%(objectname)" refs/heads/ >actual &&
	FIRST=$(head -1 actual) &&
	SECOND=$(sed -n 2p actual) &&
	test "$FIRST" "<" "$SECOND" ||
	test "$FIRST" = "$SECOND"
	)
'

test_expect_success 'sort by -objectname (reverse)' '
	(
	cd repo &&
	grit for-each-ref --sort=-objectname --format="%(objectname)" refs/heads/ >actual &&
	FIRST=$(head -1 actual) &&
	SECOND=$(sed -n 2p actual) &&
	test "$FIRST" ">" "$SECOND" ||
	test "$FIRST" = "$SECOND"
	)
'

###########################################################################
# Section 5: --merged / --no-merged (expected failures — not yet implemented)
###########################################################################

test_expect_success 'merged master returns all branches merged into master' '
	(
	cd repo &&
	grit for-each-ref --merged master --format="%(refname)" refs/heads/ >actual &&
	test_line_count = 4 actual &&
	grep "refs/heads/old" actual &&
	grep "refs/heads/mid" actual &&
	grep "refs/heads/diverge" actual &&
	grep "refs/heads/master" actual
	)
'

test_expect_success 'no-merged master returns empty (all merged in linear)' '
	(
	cd repo &&
	grit for-each-ref --no-merged master --format="%(refname)" refs/heads/ >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'merged old returns only old and diverge (not master/mid)' '
	(
	cd repo &&
	grit for-each-ref --merged old --format="%(refname)" refs/heads/ >actual &&
	test_line_count = 2 actual &&
	grep "refs/heads/old" actual &&
	grep "refs/heads/diverge" actual &&
	! grep "refs/heads/master" actual &&
	! grep "refs/heads/mid" actual
	)
'

###########################################################################
# Section 6: --contains (expected failures — not yet implemented)
###########################################################################

test_expect_success 'contains C1 returns all branches (C1 is ancestor of all)' '
	(
	cd repo &&
	C1=$(cat .c1) &&
	grit for-each-ref --contains $C1 --format="%(refname)" refs/heads/ >actual &&
	test_line_count = 4 actual &&
	grep "refs/heads/master" actual &&
	grep "refs/heads/old" actual &&
	grep "refs/heads/mid" actual &&
	grep "refs/heads/diverge" actual
	)
'

test_expect_success 'contains C3 returns only master' '
	(
	cd repo &&
	C3=$(cat .c3) &&
	grit for-each-ref --contains $C3 --format="%(refname)" refs/heads/ >actual &&
	test_line_count = 1 actual &&
	grep "refs/heads/master" actual
	)
'

###########################################################################
# Section 7: Format atoms
###########################################################################

test_expect_success 'format %(objectname) returns sha' '
	(
	cd repo &&
	grit for-each-ref --format="%(objectname)" refs/heads/master >actual &&
	grit rev-parse master >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %(objecttype) returns commit for branches' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype)" refs/heads/master >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %(refname) returns full refname' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/master >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %(refname:short) returns short name' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" refs/heads/master >actual &&
	echo "master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'default format shows oid type refname' '
	(
	cd repo &&
	grit for-each-ref refs/heads/master >actual &&
	C3=$(cat .c3) &&
	echo "$C3 commit	refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

test_done
