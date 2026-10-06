#!/bin/sh
# Tests for grit merge-base: basic ancestor, --all, --octopus,
# --is-ancestor, --independent, and multi-parent scenarios.

test_description='grit merge-base octopus, --all, --is-ancestor, --independent'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup - linear history
###########################################################################

test_expect_success 'setup linear repository' '
	(
	grit init linear &&
	cd linear &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo a >a.txt &&
	grit add . &&
	grit commit -m "root" &&
	echo b >b.txt &&
	grit add . &&
	grit commit -m "c2" &&
	echo c >c.txt &&
	grit add . &&
	grit commit -m "c3" &&
	echo d >d.txt &&
	grit add . &&
	grit commit -m "c4"
	)
'

###########################################################################
# Section 2: Basic merge-base on linear history
###########################################################################

test_expect_success 'merge-base of HEAD and HEAD~1 is HEAD~1' '
	(
	cd linear &&
	PARENT=$(grit rev-parse HEAD~1) &&
	grit merge-base HEAD HEAD~1 >actual &&
	echo "$PARENT" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'merge-base of HEAD and HEAD~2 is HEAD~2' '
	(
	cd linear &&
	GP=$(grit rev-parse HEAD~2) &&
	grit merge-base HEAD HEAD~2 >actual &&
	echo "$GP" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'merge-base of HEAD and HEAD~3 (root) is root' '
	(
	cd linear &&
	ROOT=$(grit rev-parse HEAD~3) &&
	grit merge-base HEAD HEAD~3 >actual &&
	echo "$ROOT" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'merge-base of same commit is itself' '
	(
	cd linear &&
	HEAD=$(grit rev-parse HEAD) &&
	grit merge-base HEAD HEAD >actual &&
	echo "$HEAD" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'merge-base is commutative' '
	(
	cd linear &&
	grit merge-base HEAD HEAD~2 >ab &&
	grit merge-base HEAD~2 HEAD >ba &&
	test_cmp ab ba
	)
'

test_expect_success 'merge-base of root with itself is root' '
	(
	cd linear &&
	ROOT=$(grit rev-parse HEAD~3) &&
	grit merge-base $ROOT $ROOT >actual &&
	echo "$ROOT" >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 3: --all on linear history
###########################################################################

test_expect_success 'merge-base --all on linear has single result' '
	(
	cd linear &&
	grit merge-base --all HEAD HEAD~1 >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'merge-base --all matches plain merge-base on linear' '
	(
	cd linear &&
	grit merge-base HEAD HEAD~2 >plain &&
	grit merge-base --all HEAD HEAD~2 >all &&
	test_cmp plain all
	)
'

###########################################################################
# Section 4: --octopus
###########################################################################

test_expect_success 'merge-base --octopus with two commits' '
	(
	cd linear &&
	PARENT=$(grit rev-parse HEAD~1) &&
	grit merge-base --octopus HEAD HEAD~1 >actual &&
	echo "$PARENT" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'merge-base --octopus with three commits' '
	(
	cd linear &&
	ROOT=$(grit rev-parse HEAD~3) &&
	C2=$(grit rev-parse HEAD~2) &&
	grit merge-base --octopus HEAD HEAD~1 HEAD~3 >actual &&
	echo "$ROOT" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'merge-base --octopus with four commits' '
	(
	cd linear &&
	ROOT=$(grit rev-parse HEAD~3) &&
	grit merge-base --octopus HEAD HEAD~1 HEAD~2 HEAD~3 >actual &&
	echo "$ROOT" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'merge-base --octopus all same commit' '
	(
	cd linear &&
	HEAD=$(grit rev-parse HEAD) &&
	grit merge-base --octopus HEAD HEAD HEAD >actual &&
	echo "$HEAD" >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 5: --is-ancestor
###########################################################################

test_expect_success 'is-ancestor: parent is ancestor of child' '
	(
	cd linear &&
	grit merge-base --is-ancestor HEAD~1 HEAD
	)
'

test_expect_success 'is-ancestor: root is ancestor of HEAD' '
	(
	cd linear &&
	grit merge-base --is-ancestor HEAD~3 HEAD
	)
'

test_expect_success 'is-ancestor: commit is ancestor of itself' '
	(
	cd linear &&
	grit merge-base --is-ancestor HEAD HEAD
	)
'

test_expect_success 'is-ancestor: child is NOT ancestor of parent' '
	(
	cd linear &&
	test_must_fail grit merge-base --is-ancestor HEAD HEAD~1
	)
'

test_expect_success 'is-ancestor: HEAD is NOT ancestor of root' '
	(
	cd linear &&
	test_must_fail grit merge-base --is-ancestor HEAD HEAD~3
	)
'

test_expect_success 'is-ancestor: grandparent is ancestor' '
	(
	cd linear &&
	grit merge-base --is-ancestor HEAD~2 HEAD
	)
'

test_expect_success 'is-ancestor: produces no output on success' '
	(
	cd linear &&
	grit merge-base --is-ancestor HEAD~1 HEAD >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 6: --independent
###########################################################################

test_expect_success 'independent: between HEAD and ancestor returns HEAD only' '
	(
	cd linear &&
	HEAD=$(grit rev-parse HEAD) &&
	grit merge-base --independent HEAD HEAD~2 >actual &&
	echo "$HEAD" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'independent: same commit returns empty' '
	(
	cd linear &&
	grit merge-base --independent HEAD HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'independent: three linear commits returns tip only' '
	(
	cd linear &&
	HEAD=$(grit rev-parse HEAD) &&
	grit merge-base --independent HEAD HEAD~1 HEAD~2 >actual &&
	echo "$HEAD" >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 7: Branched topology
###########################################################################

test_expect_success 'setup branched repository' '
	(
	grit init branched &&
	cd branched &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo base >base.txt &&
	grit add . &&
	grit commit -m "base" &&
	grit branch side &&
	echo main1 >main1.txt &&
	grit add . &&
	grit commit -m "main1" &&
	echo main2 >main2.txt &&
	grit add . &&
	grit commit -m "main2"
	)
'

test_expect_success 'merge-base of master and side is fork point' '
	(
	cd branched &&
	FORK=$(grit rev-parse HEAD~2) &&
	grit merge-base HEAD side >actual &&
	echo "$FORK" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'merge-base --all of master and side' '
	(
	cd branched &&
	FORK=$(grit rev-parse HEAD~2) &&
	grit merge-base --all HEAD side >actual &&
	echo "$FORK" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'is-ancestor: side is not ancestor of master tip' '
	(
	cd branched &&
	SIDE=$(grit rev-parse side) &&
	FORK=$(grit rev-parse HEAD~2) &&
	# side points to base (fork), which IS an ancestor
	grit merge-base --is-ancestor side HEAD
	)
'

test_expect_success 'is-ancestor: master tip is not ancestor of side' '
	(
	cd branched &&
	test_must_fail grit merge-base --is-ancestor HEAD side
	)
'

test_expect_success 'octopus of master and side is base' '
	(
	cd branched &&
	FORK=$(grit rev-parse HEAD~2) &&
	grit merge-base --octopus HEAD side >actual &&
	echo "$FORK" >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 8: Cross-check with real git
###########################################################################

test_expect_success 'setup cross-check repo' '
	(
	$REAL_GIT init cross &&
	cd cross &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo one >one.txt &&
	$REAL_GIT add . &&
	$REAL_GIT commit -m "one" &&
	$REAL_GIT branch br1 &&
	echo two >two.txt &&
	$REAL_GIT add . &&
	$REAL_GIT commit -m "two" &&
	echo three >three.txt &&
	$REAL_GIT add . &&
	$REAL_GIT commit -m "three"
	)
'

test_expect_success 'merge-base matches real git for linear history' '
	(
	cd cross &&
	grit merge-base HEAD HEAD~1 >grit_out &&
	$REAL_GIT merge-base HEAD HEAD~1 >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'merge-base --all matches real git' '
	(
	cd cross &&
	grit merge-base --all HEAD HEAD~2 >grit_out &&
	$REAL_GIT merge-base --all HEAD HEAD~2 >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'merge-base --octopus matches real git' '
	(
	cd cross &&
	grit merge-base --octopus HEAD HEAD~1 HEAD~2 >grit_out &&
	$REAL_GIT merge-base --octopus HEAD HEAD~1 HEAD~2 >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'merge-base of master and branch matches real git' '
	(
	cd cross &&
	grit merge-base HEAD br1 >grit_out &&
	$REAL_GIT merge-base HEAD br1 >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'merge-base --is-ancestor matches real git' '
	(
	cd cross &&
	grit merge-base --is-ancestor HEAD~1 HEAD &&
	$REAL_GIT merge-base --is-ancestor HEAD~1 HEAD
	)
'

test_done
