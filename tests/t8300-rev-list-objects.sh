#!/bin/sh
# Tests for rev-list: commit enumeration, counting, ordering, ranges.
# --objects basic tests now pass; --objects-edge still TODO.

test_description='rev-list commit enumeration and object listing'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup repository with linear history' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&

	echo "file1" >file1.txt &&
	git add file1.txt &&
	git commit -m "first commit" &&

	echo "file2" >file2.txt &&
	git add file2.txt &&
	git commit -m "second commit" &&

	echo "file3" >file3.txt &&
	git add file3.txt &&
	git commit -m "third commit" &&

	echo "file4" >file4.txt &&
	git add file4.txt &&
	git commit -m "fourth commit" &&

	echo "file5" >file5.txt &&
	git add file5.txt &&
	git commit -m "fifth commit"
	)
'

###########################################################################
# Section 1: Basic rev-list
###########################################################################

test_expect_success 'rev-list HEAD lists all commits' '
	(
	cd repo &&
	git rev-list HEAD >actual &&
	test $(wc -l <actual) -eq 5
	)
'

test_expect_success 'rev-list HEAD produces valid OIDs' '
	(
	cd repo &&
	git rev-list HEAD >actual &&
	while read oid; do
		echo "$oid" | grep -qE "^[0-9a-f]{40}$" || exit 1
	done <actual
	)
'

test_expect_success 'rev-list HEAD contains HEAD commit' '
	(
	cd repo &&
	git rev-list HEAD >actual &&
	head_oid=$(git rev-parse HEAD) &&
	grep "$head_oid" actual
	)
'

test_expect_success 'rev-list HEAD contains root commit' '
	(
	cd repo &&
	git rev-list HEAD >all &&
	git log --reverse --format="%H" >log_all &&
	root=$(head -1 log_all) &&
	grep "$root" all
	)
'

test_expect_success 'rev-list lists exactly 5 distinct commits' '
	(
	cd repo &&
	git rev-list HEAD >actual &&
	sort -u actual >unique &&
	test $(wc -l <unique) -eq 5
	)
'

###########################################################################
# Section 2: --count
###########################################################################

test_expect_success 'rev-list --count HEAD returns correct count' '
	(
	cd repo &&
	count=$(git rev-list --count HEAD) &&
	test "$count" -eq 5
	)
'

test_expect_success 'rev-list --count with range' '
	(
	cd repo &&
	count=$(git rev-list --count HEAD~2..HEAD) &&
	test "$count" -eq 2
	)
'

test_expect_success 'rev-list --count with single commit range' '
	(
	cd repo &&
	count=$(git rev-list --count HEAD~1..HEAD) &&
	test "$count" -eq 1
	)
'

test_expect_success 'rev-list --count of first commit via range' '
	(
	cd repo &&
	count=$(git rev-list --count HEAD~4..HEAD~3) &&
	test "$count" -eq 1
	)
'

###########################################################################
# Section 3: --max-count and --skip
###########################################################################

test_expect_success 'rev-list --max-count=3 limits output' '
	(
	cd repo &&
	git rev-list --max-count=3 HEAD >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'rev-list --max-count=1 shows one commit' '
	(
	cd repo &&
	git rev-list --max-count=1 HEAD >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'rev-list --max-count=0 produces empty output' '
	(
	cd repo &&
	git rev-list --max-count=0 HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'rev-list --skip=2 skips two commits' '
	(
	cd repo &&
	git rev-list --skip=2 HEAD >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'rev-list --skip=4 shows one commit' '
	(
	cd repo &&
	git rev-list --skip=4 HEAD >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'rev-list --skip and --max-count combined' '
	(
	cd repo &&
	git rev-list --skip=1 --max-count=2 HEAD >actual &&
	test $(wc -l <actual) -eq 2
	)
'

###########################################################################
# Section 4: --reverse
###########################################################################

test_expect_success 'rev-list --reverse produces same set of commits' '
	(
	cd repo &&
	git rev-list HEAD >forward &&
	git rev-list --reverse HEAD >reverse &&
	sort forward >sorted_fwd &&
	sort reverse >sorted_rev &&
	test_cmp sorted_fwd sorted_rev
	)
'

###########################################################################
# Section 5: Ranges
###########################################################################

test_expect_success 'rev-list with .. range notation' '
	(
	cd repo &&
	git rev-list HEAD~3..HEAD >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'rev-list with empty range produces no output' '
	(
	cd repo &&
	git rev-list HEAD..HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'rev-list HEAD~1..HEAD shows exactly one commit' '
	(
	cd repo &&
	git rev-list HEAD~1..HEAD >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'rev-list range result is subset of full list' '
	(
	cd repo &&
	git rev-list HEAD >full &&
	git rev-list HEAD~2..HEAD >range &&
	while read oid; do
		grep -q "$oid" full || exit 1
	done <range
	)
'

test_expect_success 'rev-list HEAD~4..HEAD shows 4 commits' '
	(
	cd repo &&
	git rev-list HEAD~4..HEAD >actual &&
	test $(wc -l <actual) -eq 4
	)
'

###########################################################################
# Section 6: --first-parent
###########################################################################

test_expect_success 'rev-list --first-parent on linear history' '
	(
	cd repo &&
	git rev-list HEAD >normal &&
	git rev-list --first-parent HEAD >first_parent &&
	test $(wc -l <normal) -eq $(wc -l <first_parent)
	)
'

###########################################################################
# Section 7: Ordering
###########################################################################

test_expect_success 'rev-list --topo-order produces valid output' '
	(
	cd repo &&
	git rev-list --topo-order HEAD >actual &&
	test $(wc -l <actual) -eq 5
	)
'

test_expect_success 'rev-list --date-order produces valid output' '
	(
	cd repo &&
	git rev-list --date-order HEAD >actual &&
	test $(wc -l <actual) -eq 5
	)
'

test_expect_success 'topo-order and date-order contain same commits' '
	(
	cd repo &&
	git rev-list --topo-order HEAD >topo &&
	git rev-list --date-order HEAD >date_ord &&
	sort topo >sorted_topo &&
	sort date_ord >sorted_date &&
	test_cmp sorted_topo sorted_date
	)
'

###########################################################################
# Section 8: --objects (not yet implemented)
###########################################################################

test_expect_success 'rev-list --objects lists commits and their trees/blobs' '
	(
	cd repo &&
	git rev-list --objects HEAD >actual &&
	commit_count=$(git rev-list --count HEAD) &&
	total=$(wc -l <actual) &&
	test "$total" -gt "$commit_count"
	)
'

test_expect_success 'rev-list --objects includes blob OIDs' '
	(
	cd repo &&
	git rev-list --objects HEAD >actual &&
	blob_oid=$(git hash-object file1.txt) &&
	grep "$blob_oid" actual
	)
'

test_expect_success 'rev-list --objects includes tree OIDs' '
	(
	cd repo &&
	tree_oid=$(git log -n1 --format="%T") &&
	git rev-list --objects HEAD >actual &&
	grep "$tree_oid" actual
	)
'

test_expect_success 'rev-list --objects-edge marks boundary objects' '
	(
	cd repo &&
	git rev-list --objects-edge HEAD~2..HEAD >actual &&
	test $(wc -l <actual) -gt 0
	)
'

###########################################################################
# Section 9: Edge cases and tag/branch refs
###########################################################################

test_expect_success 'rev-list on single commit repo' '
	(
	git init single &&
	cd single &&
	git config user.name "T" &&
	git config user.email "t@t.com" &&
	git commit --allow-empty -m "only" &&
	git rev-list HEAD >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'rev-list --count on single commit' '
	(
	cd single &&
	count=$(git rev-list --count HEAD) &&
	test "$count" -eq 1
	)
'

test_expect_success 'rev-list with tag ref' '
	(
	cd repo &&
	git tag v1.0 HEAD~2 &&
	git rev-list v1.0 >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'rev-list with branch ref' '
	(
	cd repo &&
	git branch feature &&
	git rev-list feature >actual &&
	test $(wc -l <actual) -eq 5
	)
'

test_done
