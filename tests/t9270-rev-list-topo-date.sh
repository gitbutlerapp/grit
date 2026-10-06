#!/bin/sh
# Test rev-list with --topo-order, --date-order, --reverse, --count,
# --first-parent, range syntax, and merge topologies.

test_description='grit rev-list topo-order and date-order'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Setup: linear history with known timestamps
###########################################################################

test_expect_success 'setup linear history' '
	(
	grit init --initial-branch=master repo &&
	cd repo &&
	$REAL_GIT config user.email "test@test.com" &&
	$REAL_GIT config user.name "Tester" &&
	echo "first" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "C1" &&
	grit rev-parse HEAD >../SHA_C1 &&
	echo "second" >>file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "C2" &&
	grit rev-parse HEAD >../SHA_C2 &&
	echo "third" >>file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "C3" &&
	grit rev-parse HEAD >../SHA_C3 &&
	echo "fourth" >>file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "C4" &&
	grit rev-parse HEAD >../SHA_C4
	)
'

###########################################################################
# Section 1: Basic rev-list
###########################################################################

test_expect_success 'rev-list HEAD lists 4 commits' '
	(
	cd repo &&
	grit rev-list HEAD >out &&
	test_line_count = 4 out
	)
'

test_expect_success 'rev-list HEAD shows full 40-char hashes' '
	(
	cd repo &&
	grit rev-list HEAD >out &&
	hash=$(head -1 out) &&
	test ${#hash} -eq 40
	)
'

test_expect_success 'rev-list HEAD includes all known SHAs' '
	(
	cd repo &&
	grit rev-list HEAD >out &&
	grep $(cat ../SHA_C1) out &&
	grep $(cat ../SHA_C2) out &&
	grep $(cat ../SHA_C3) out &&
	grep $(cat ../SHA_C4) out
	)
'

test_expect_success 'rev-list HEAD newest first (default order)' '
	(
	cd repo &&
	grit rev-list HEAD >out &&
	test "$(head -1 out)" = "$(cat ../SHA_C4)" &&
	test "$(tail -1 out)" = "$(cat ../SHA_C1)"
	)
'

###########################################################################
# Section 2: --count
###########################################################################

test_expect_success 'rev-list --count HEAD returns 4' '
	(
	cd repo &&
	count=$(grit rev-list --count HEAD) &&
	test "$count" = "4"
	)
'

test_expect_success 'rev-list --count with older commit' '
	(
	cd repo &&
	count=$(grit rev-list --count $(cat ../SHA_C2)) &&
	test "$count" = "2"
	)
'

test_expect_success 'rev-list --count with root commit returns 1' '
	(
	cd repo &&
	count=$(grit rev-list --count $(cat ../SHA_C1)) &&
	test "$count" = "1"
	)
'

###########################################################################
# Section 3: --reverse
###########################################################################

test_expect_success 'rev-list --reverse shows oldest first' '
	(
	cd repo &&
	grit rev-list --reverse HEAD >out &&
	test "$(head -1 out)" = "$(cat ../SHA_C1)" &&
	test "$(tail -1 out)" = "$(cat ../SHA_C4)"
	)
'

test_expect_success 'rev-list --reverse has same count as normal' '
	(
	cd repo &&
	grit rev-list HEAD >normal &&
	grit rev-list --reverse HEAD >reversed &&
	normal_count=$(wc -l <normal) &&
	reversed_count=$(wc -l <reversed) &&
	test "$normal_count" = "$reversed_count"
	)
'

###########################################################################
# Section 4: --topo-order on linear history
###########################################################################

test_expect_success 'rev-list --topo-order on linear history matches default' '
	(
	cd repo &&
	grit rev-list HEAD >default_out &&
	grit rev-list --topo-order HEAD >topo_out &&
	test_cmp default_out topo_out
	)
'

test_expect_success 'rev-list --topo-order --count matches' '
	(
	cd repo &&
	count=$(grit rev-list --topo-order --count HEAD) &&
	test "$count" = "4"
	)
'

###########################################################################
# Section 5: --date-order on linear history
###########################################################################

test_expect_success 'rev-list --date-order on linear history matches default' '
	(
	cd repo &&
	grit rev-list HEAD >default_out &&
	grit rev-list --date-order HEAD >date_out &&
	test_cmp default_out date_out
	)
'

test_expect_success 'rev-list --date-order --count matches' '
	(
	cd repo &&
	count=$(grit rev-list --date-order --count HEAD) &&
	test "$count" = "4"
	)
'

###########################################################################
# Section 6: Range syntax
###########################################################################

test_expect_success 'rev-list with .. range excludes older commits' '
	(
	cd repo &&
	grit rev-list $(cat ../SHA_C2)..HEAD >out &&
	test_line_count = 2 out &&
	grep $(cat ../SHA_C4) out &&
	grep $(cat ../SHA_C3) out &&
	! grep $(cat ../SHA_C2) out
	)
'

test_expect_success 'rev-list --count with range' '
	(
	cd repo &&
	count=$(grit rev-list --count $(cat ../SHA_C2)..HEAD) &&
	test "$count" = "2"
	)
'

test_expect_success 'rev-list with ^ exclusion matches .. range' '
	(
	cd repo &&
	grit rev-list $(cat ../SHA_C2)..HEAD >range_out &&
	grit rev-list HEAD ^$(cat ../SHA_C2) >caret_out &&
	test_cmp range_out caret_out
	)
'

test_expect_success 'rev-list single-commit range returns 1' '
	(
	cd repo &&
	grit rev-list $(cat ../SHA_C3)..$(cat ../SHA_C4) >out &&
	test_line_count = 1 out
	)
'

###########################################################################
# Section 7: Setup branched/merge topology
###########################################################################

test_expect_success 'setup branch and merge' '
	(
	cd repo &&
	$REAL_GIT checkout -b side $(cat ../SHA_C2) &&
	echo "side1" >side.txt &&
	grit add side.txt &&
	test_tick &&
	grit commit -m "S1" &&
	grit rev-parse HEAD >../SHA_S1 &&
	echo "side2" >>side.txt &&
	grit add side.txt &&
	test_tick &&
	grit commit -m "S2" &&
	grit rev-parse HEAD >../SHA_S2 &&
	$REAL_GIT checkout master &&
	$REAL_GIT merge side --no-edit &&
	grit rev-parse HEAD >../SHA_MERGE
	)
'

###########################################################################
# Section 8: --topo-order with merge
###########################################################################

test_expect_success 'rev-list --topo-order includes merge commit' '
	(
	cd repo &&
	grit rev-list --topo-order HEAD >out &&
	grep $(cat ../SHA_MERGE) out
	)
'

test_expect_success 'rev-list --topo-order includes side branch commits' '
	(
	cd repo &&
	grit rev-list --topo-order HEAD >out &&
	grep $(cat ../SHA_S1) out &&
	grep $(cat ../SHA_S2) out
	)
'

test_expect_success 'rev-list --topo-order groups side branch together' '
	(
	cd repo &&
	grit rev-list --topo-order HEAD >out &&
	s1_line=$(grep -n $(cat ../SHA_S1) out | cut -d: -f1) &&
	s2_line=$(grep -n $(cat ../SHA_S2) out | cut -d: -f1) &&
	diff=$((s2_line - s1_line)) &&
	# In topo order, S1 and S2 should be adjacent (diff = 1 or -1)
	test "$diff" = "1" || test "$diff" = "-1"
	)
'

###########################################################################
# Section 9: --date-order with merge
###########################################################################

test_expect_success 'rev-list --date-order includes all commits' '
	(
	cd repo &&
	grit rev-list --date-order HEAD >out &&
	grep $(cat ../SHA_C1) out &&
	grep $(cat ../SHA_S1) out &&
	grep $(cat ../SHA_MERGE) out
	)
'

test_expect_success 'rev-list --date-order --count matches total' '
	(
	cd repo &&
	total=$(grit rev-list --count HEAD) &&
	date_count=$(grit rev-list --date-order --count HEAD) &&
	test "$total" = "$date_count"
	)
'

###########################################################################
# Section 10: --first-parent with merge
###########################################################################

test_expect_success 'rev-list --first-parent excludes side branch' '
	(
	cd repo &&
	grit rev-list --first-parent HEAD >out &&
	! grep $(cat ../SHA_S1) out &&
	! grep $(cat ../SHA_S2) out
	)
'

test_expect_success 'rev-list --first-parent includes merge and master commits' '
	(
	cd repo &&
	grit rev-list --first-parent HEAD >out &&
	grep $(cat ../SHA_MERGE) out &&
	grep $(cat ../SHA_C4) out &&
	grep $(cat ../SHA_C1) out
	)
'

test_expect_success 'rev-list --first-parent --count is less than total' '
	(
	cd repo &&
	total=$(grit rev-list --count HEAD) &&
	fp_count=$(grit rev-list --first-parent --count HEAD) &&
	test "$fp_count" -lt "$total"
	)
'

test_expect_success 'rev-list --first-parent --reverse shows oldest first' '
	(
	cd repo &&
	grit rev-list --first-parent --reverse HEAD >out &&
	test "$(head -1 out)" = "$(cat ../SHA_C1)"
	)
'

###########################################################################
# Section 11: Combining options
###########################################################################

test_expect_success 'rev-list --topo-order --reverse flips topo output' '
	(
	cd repo &&
	grit rev-list --topo-order HEAD >topo &&
	grit rev-list --topo-order --reverse HEAD >topo_rev &&
	# First of reversed should be last of normal
	test "$(head -1 topo_rev)" = "$(tail -1 topo)"
	)
'

test_expect_success 'rev-list --date-order --reverse flips date output' '
	(
	cd repo &&
	grit rev-list --date-order HEAD >date &&
	grit rev-list --date-order --reverse HEAD >date_rev &&
	test "$(head -1 date_rev)" = "$(tail -1 date)"
	)
'

test_expect_success 'rev-list range with --topo-order' '
	(
	cd repo &&
	grit rev-list --topo-order $(cat ../SHA_C2)..HEAD >out &&
	test -s out &&
	! grep $(cat ../SHA_C2) out &&
	! grep $(cat ../SHA_C1) out
	)
'

test_done
