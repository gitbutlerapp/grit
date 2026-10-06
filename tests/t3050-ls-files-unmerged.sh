#!/bin/sh
# Test ls-files with unmerged/conflict entries (-u flag).

test_description='grit ls-files with unmerged/conflict entries'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup helper - create conflict scenarios
###########################################################################

test_expect_success 'setup: create base/ours/theirs for single conflict' '
	(
	grit init repo &&
	cd repo &&

	echo "base" >conflict.txt &&
	echo "stable" >clean.txt &&
	rm -f .git/index &&
	grit update-index --add conflict.txt clean.txt &&
	tree_base=$(grit write-tree) &&
	echo "$tree_base" >../tree_base &&

	echo "ours" >conflict.txt &&
	echo "stable" >clean.txt &&
	rm -f .git/index &&
	grit update-index --add conflict.txt clean.txt &&
	tree_ours=$(grit write-tree) &&
	echo "$tree_ours" >../tree_ours &&

	echo "theirs" >conflict.txt &&
	echo "stable" >clean.txt &&
	rm -f .git/index &&
	grit update-index --add conflict.txt clean.txt &&
	tree_theirs=$(grit write-tree) &&
	echo "$tree_theirs" >../tree_theirs
	)
'

test_expect_success 'create merge with conflict' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree -m "$(cat ../tree_base)" "$(cat ../tree_ours)" "$(cat ../tree_theirs)"
	)
'

###########################################################################
# Section 2: ls-files -u basics
###########################################################################

test_expect_success 'ls-files -u shows unmerged entries' '
	(
	cd repo &&
	grit ls-files -u >actual &&
	test -s actual
	)
'

test_expect_success 'ls-files -u lists only conflicting file' '
	(
	cd repo &&
	grit ls-files -u >actual &&
	grep "conflict.txt" actual &&
	! grep "clean.txt" actual
	)
'

test_expect_success 'ls-files -u shows 3 entries for conflicted file' '
	(
	cd repo &&
	grit ls-files -u >actual &&
	count=$(wc -l <actual | tr -d " ") &&
	test "$count" = "3"
	)
'

test_expect_success 'ls-files -u entries have stages 1, 2, 3' '
	(
	cd repo &&
	grit ls-files -u >actual &&
	grep "	conflict.txt" actual >conflict_lines &&
	awk "{print \$3}" conflict_lines | sort >stages &&
	printf "1\n2\n3\n" >expect &&
	test_cmp expect stages
	)
'

test_expect_success 'ls-files -u entries have mode 100644' '
	(
	cd repo &&
	grit ls-files -u >actual &&
	while IFS= read -r line; do
		mode=$(echo "$line" | awk "{print \$1}") &&
		test "$mode" = "100644" || return 1
	done <actual
	)
'

test_expect_success 'ls-files -u entries have valid OIDs (40 hex chars)' '
	(
	cd repo &&
	grit ls-files -u >actual &&
	while IFS= read -r line; do
		oid=$(echo "$line" | awk "{print \$2}") &&
		echo "$oid" | grep -qE "^[0-9a-f]{40}$" || return 1
	done <actual
	)
'

###########################################################################
# Section 3: ls-files --stage with unmerged entries
###########################################################################

test_expect_success 'ls-files -s shows merged and unmerged stage entries' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	grep "clean.txt" actual &&
	grep "1.conflict.txt" actual &&
	grep "2.conflict.txt" actual &&
	grep "3.conflict.txt" actual
	)
'

test_expect_success 'clean file shows stage 0 in -s output' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	grep "0.clean.txt" actual
	)
'

test_expect_success 'ls-files -u shows conflict entries with all 3 stages' '
	(
	cd repo &&
	grit ls-files -u >actual &&
	count=$(grep -c "conflict.txt" actual) &&
	test "$count" = "3" &&
	grep "1.conflict.txt" actual &&
	grep "2.conflict.txt" actual &&
	grep "3.conflict.txt" actual
	)
'

###########################################################################
# Section 4: ls-files -u with no conflicts
###########################################################################

test_expect_success 'ls-files -u on clean index is empty' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree "$(cat ../tree_base)" &&
	grit ls-files -u >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'ls-files -u on freshly init repo is empty' '
	(
	grit init clean-repo &&
	cd clean-repo &&
	grit ls-files -u >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 5: Multiple conflicting files
###########################################################################

test_expect_success 'setup: trees with 3 conflicting files' '
	(
	cd repo &&

	rm -f .git/index &&
	echo "base A" >a.txt &&
	echo "base B" >b.txt &&
	echo "base C" >c.txt &&
	echo "clean" >d.txt &&
	grit update-index --add a.txt b.txt c.txt d.txt &&
	tree_multi_base=$(grit write-tree) &&
	echo "$tree_multi_base" >../tree_multi_base &&

	rm -f .git/index &&
	echo "ours A" >a.txt &&
	echo "ours B" >b.txt &&
	echo "ours C" >c.txt &&
	echo "clean" >d.txt &&
	grit update-index --add a.txt b.txt c.txt d.txt &&
	tree_multi_ours=$(grit write-tree) &&
	echo "$tree_multi_ours" >../tree_multi_ours &&

	rm -f .git/index &&
	echo "theirs A" >a.txt &&
	echo "theirs B" >b.txt &&
	echo "theirs C" >c.txt &&
	echo "clean" >d.txt &&
	grit update-index --add a.txt b.txt c.txt d.txt &&
	tree_multi_theirs=$(grit write-tree) &&
	echo "$tree_multi_theirs" >../tree_multi_theirs
	)
'

test_expect_success 'ls-files -u shows all 3 conflicting files' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree -m "$(cat ../tree_multi_base)" "$(cat ../tree_multi_ours)" "$(cat ../tree_multi_theirs)" &&
	grit ls-files -u >actual &&
	grep "a.txt" actual &&
	grep "b.txt" actual &&
	grep "c.txt" actual
	)
'

test_expect_success 'ls-files -u has 9 entries for 3 conflicting files' '
	(
	cd repo &&
	grit ls-files -u >actual &&
	count=$(wc -l <actual | tr -d " ") &&
	test "$count" = "9"
	)
'

test_expect_success 'ls-files -u does not show clean file d.txt' '
	(
	cd repo &&
	grit ls-files -u >actual &&
	! grep "d.txt" actual
	)
'

test_expect_success 'ls-files -u with pathspec filters to one file' '
	(
	cd repo &&
	grit ls-files -u a.txt >actual &&
	grep "a.txt" actual &&
	! grep "b.txt" actual &&
	! grep "c.txt" actual
	)
'

test_expect_success 'ls-files -u pathspec on clean file is empty' '
	(
	cd repo &&
	grit ls-files -u d.txt >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 6: Deletion conflicts
###########################################################################

test_expect_success 'setup: delete-vs-modify conflict' '
	(
	cd repo &&

	rm -f .git/index &&
	echo "delete me" >del.txt &&
	echo "keep" >keep.txt &&
	grit update-index --add del.txt keep.txt &&
	tree_del_base=$(grit write-tree) &&
	echo "$tree_del_base" >../tree_del_base &&

	# Ours: delete del.txt
	rm -f .git/index &&
	echo "keep" >keep.txt &&
	grit update-index --add keep.txt &&
	tree_del_ours=$(grit write-tree) &&
	echo "$tree_del_ours" >../tree_del_ours &&

	# Theirs: modify del.txt
	rm -f .git/index &&
	echo "modified" >del.txt &&
	echo "keep" >keep.txt &&
	grit update-index --add del.txt keep.txt &&
	tree_del_theirs=$(grit write-tree) &&
	echo "$tree_del_theirs" >../tree_del_theirs
	)
'

test_expect_success 'delete-vs-modify creates unmerged entries' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree -m "$(cat ../tree_del_base)" "$(cat ../tree_del_ours)" "$(cat ../tree_del_theirs)" &&
	grit ls-files -u >actual &&
	grep "del.txt" actual
	)
'

test_expect_success 'delete-vs-modify: stage 1 and 3 present, stage 2 absent' '
	(
	cd repo &&
	grit ls-files -u >actual &&
	grep "1.del.txt" actual &&
	grep "3.del.txt" actual &&
	! grep "2.del.txt" actual
	)
'

###########################################################################
# Section 7: Reset clears unmerged, ls-files -u reflects it
###########################################################################

test_expect_success 'read-tree --reset makes ls-files -u empty' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree -m "$(cat ../tree_multi_base)" "$(cat ../tree_multi_ours)" "$(cat ../tree_multi_theirs)" &&
	grit ls-files -u >before &&
	test -s before &&
	grit read-tree --reset "$(cat ../tree_multi_base)" &&
	grit ls-files -u >after &&
	test_must_be_empty after
	)
'

test_done
