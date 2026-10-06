#!/bin/sh
# Tests for rev-list ordering with merge commits.

test_description='rev-list ordering with merge commits'

. ./test-lib.sh

GIT_AUTHOR_NAME='A U Thor'
GIT_AUTHOR_EMAIL='author@example.com'
GIT_COMMITTER_NAME='C O Mmiter'
GIT_COMMITTER_EMAIL='committer@example.com'
export GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL

test_expect_success 'setup: create repo with diamond merge' '
	(
	git init merge-repo &&
	cd merge-repo &&
	TREE=$(git write-tree) &&
	A=$(echo A | git commit-tree "$TREE") &&
	B=$(echo B | git commit-tree "$TREE" -p "$A") &&
	C=$(echo C | git commit-tree "$TREE" -p "$A") &&
	MERGE=$(echo M | git commit-tree "$TREE" -p "$B" -p "$C") &&
	D=$(echo D | git commit-tree "$TREE" -p "$MERGE") &&
	git update-ref refs/heads/master "$D" &&
	git update-ref refs/tags/tagA "$A" &&
	git update-ref refs/tags/tagB "$B" &&
	git update-ref refs/tags/tagC "$C" &&
	git update-ref refs/tags/tagM "$MERGE" &&
	git update-ref refs/tags/tagD "$D"
	)
'

test_expect_success 'rev-list walks all commits through merge' '
	(
	cd merge-repo &&
	git rev-list master >actual &&
	test_line_count = 5 actual
	)
'

test_expect_success 'all five commits are unique' '
	(
	cd merge-repo &&
	git rev-list master | sort -u >unique &&
	test_line_count = 5 unique
	)
'

test_expect_success '--first-parent skips second parent branch' '
	(
	cd merge-repo &&
	C=$(git rev-parse tagC) &&
	git rev-list --first-parent master >fp &&
	! grep -q "$C" fp
	)
'

test_expect_success '--first-parent still includes merge commit' '
	(
	cd merge-repo &&
	MERGE=$(git rev-parse tagM) &&
	git rev-list --first-parent master >fp &&
	grep -q "$MERGE" fp
	)
'

test_expect_success '--first-parent count is less than or equal to full walk' '
	(
	cd merge-repo &&
	git rev-list master >full &&
	git rev-list --first-parent master >fp &&
	full_c=$(wc -l <full | tr -d " ") &&
	fp_c=$(wc -l <fp | tr -d " ") &&
	test "$fp_c" -le "$full_c"
	)
'

test_expect_success '--count counts all commits including merge' '
	(
	cd merge-repo &&
	count=$(git rev-list --count master) &&
	test "$count" = "5"
	)
'

test_expect_success '--count --first-parent counts only first-parent chain' '
	(
	cd merge-repo &&
	git rev-list --first-parent master >fp &&
	fp_lines=$(wc -l <fp | tr -d " ") &&
	count=$(git rev-list --count --first-parent master) &&
	test "$count" = "$fp_lines"
	)
'

test_expect_success 'range excluding root shows all but root' '
	(
	cd merge-repo &&
	A=$(git rev-parse tagA) &&
	git rev-list tagA..master >actual &&
	! grep -q "$A" actual &&
	test_line_count = 4 actual
	)
'

test_expect_success '--topo-order walks all commits' '
	(
	cd merge-repo &&
	git rev-list --topo-order master >topo &&
	git rev-list master >default_list &&
	sort topo >s1 &&
	sort default_list >s2 &&
	test_cmp s1 s2
	)
'

test_expect_success '--date-order walks all commits' '
	(
	cd merge-repo &&
	git rev-list --date-order master >date_list &&
	git rev-list master >default_list &&
	sort date_list >s1 &&
	sort default_list >s2 &&
	test_cmp s1 s2
	)
'

test_expect_success '--topo-order: merge is present in output' '
	(
	cd merge-repo &&
	MERGE=$(git rev-parse tagM) &&
	git rev-list --topo-order master >topo &&
	grep -q "$MERGE" topo
	)
'

test_expect_success '--parents shows parent info on merge line' '
	(
	cd merge-repo &&
	MERGE=$(git rev-parse tagM) &&
	git rev-list --parents master >parents_out &&
	merge_line=$(grep "^$MERGE" parents_out) &&
	# Merge commit line should have 3 hashes (self + 2 parents)
	word_count=$(echo "$merge_line" | wc -w | tr -d " ") &&
	test "$word_count" = "3"
	)
'

test_expect_success '--parents shows single parent for non-merge' '
	(
	cd merge-repo &&
	D=$(git rev-parse tagD) &&
	git rev-list --parents master >parents_out &&
	d_line=$(grep "^$D" parents_out) &&
	word_count=$(echo "$d_line" | wc -w | tr -d " ") &&
	test "$word_count" = "2"
	)
'

test_expect_success '--parents root commit has just itself' '
	(
	cd merge-repo &&
	A=$(git rev-parse tagA) &&
	git rev-list --parents master >parents_out &&
	a_line=$(grep "^$A" parents_out) &&
	word_count=$(echo "$a_line" | wc -w | tr -d " ") &&
	test "$word_count" = "1"
	)
'

test_expect_success 'setup: octopus merge (3 parents)' '
	(
	cd merge-repo &&
	TREE=$(git write-tree) &&
	A=$(git rev-parse tagA) &&
	E=$(echo E | git commit-tree "$TREE" -p "$A") &&
	F=$(echo F | git commit-tree "$TREE" -p "$A") &&
	G=$(echo G | git commit-tree "$TREE" -p "$A") &&
	OCTO=$(echo OCTO | git commit-tree "$TREE" -p "$E" -p "$F" -p "$G") &&
	git update-ref refs/heads/octopus "$OCTO" &&
	git update-ref refs/tags/tagOCTO "$OCTO" &&
	git update-ref refs/tags/tagE "$E" &&
	git update-ref refs/tags/tagF "$F" &&
	git update-ref refs/tags/tagG "$G"
	)
'

test_expect_success 'octopus merge: all commits in walk' '
	(
	cd merge-repo &&
	git rev-list octopus >actual &&
	test_line_count = 5 actual
	)
'

test_expect_success '--parents: octopus has 3 parents' '
	(
	cd merge-repo &&
	OCTO=$(git rev-parse tagOCTO) &&
	git rev-list --parents octopus >parents_out &&
	octo_line=$(grep "^$OCTO" parents_out) &&
	word_count=$(echo "$octo_line" | wc -w | tr -d " ") &&
	test "$word_count" = "4"
	)
'

test_expect_success '--first-parent on octopus skips 2nd and 3rd parents' '
	(
	cd merge-repo &&
	F=$(git rev-parse tagF) &&
	G=$(git rev-parse tagG) &&
	git rev-list --first-parent octopus >fp &&
	! grep -q "$F" fp &&
	! grep -q "$G" fp
	)
'

test_expect_success '--max-count=1 on merge branch returns one commit' '
	(
	cd merge-repo &&
	git rev-list --max-count=1 master >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success '--max-count=3 limits output to 3' '
	(
	cd merge-repo &&
	git rev-list --max-count=3 master >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success '--skip=2 skips first 2 commits' '
	(
	cd merge-repo &&
	git rev-list master >full &&
	git rev-list --skip=2 master >skipped &&
	full_c=$(wc -l <full | tr -d " ") &&
	skip_c=$(wc -l <skipped | tr -d " ") &&
	expected=$(($full_c - 2)) &&
	test "$skip_c" = "$expected"
	)
'

test_expect_success '--skip and --max-count combined on merge branch' '
	(
	cd merge-repo &&
	git rev-list --skip=1 --max-count=2 master >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success '--quiet produces no output' '
	(
	cd merge-repo &&
	git rev-list --quiet master >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'setup: chain of merges' '
	(
	cd merge-repo &&
	TREE=$(git write-tree) &&
	A=$(git rev-parse tagA) &&
	X1=$(echo X1 | git commit-tree "$TREE" -p "$A") &&
	Y1=$(echo Y1 | git commit-tree "$TREE" -p "$A") &&
	M1=$(echo M1 | git commit-tree "$TREE" -p "$X1" -p "$Y1") &&
	X2=$(echo X2 | git commit-tree "$TREE" -p "$M1") &&
	Y2=$(echo Y2 | git commit-tree "$TREE" -p "$M1") &&
	M2=$(echo M2 | git commit-tree "$TREE" -p "$X2" -p "$Y2") &&
	git update-ref refs/heads/chain "$M2"
	)
'

test_expect_success 'chain of merges: full walk has 7 commits' '
	(
	cd merge-repo &&
	git rev-list chain >actual &&
	test_line_count = 7 actual
	)
'

test_expect_success '--first-parent through chain of merges' '
	(
	cd merge-repo &&
	git rev-list --first-parent chain >fp &&
	# A -> X1 -> M1 -> X2 -> M2 = 5
	test_line_count = 5 fp
	)
'

test_expect_success '^A B excludes A from output' '
	(
	cd merge-repo &&
	A=$(git rev-parse tagA) &&
	git rev-list ^tagA master >actual &&
	! grep -q "$A" actual
	)
'

test_expect_success 'A..B is same as ^A B' '
	(
	cd merge-repo &&
	git rev-list tagA..master >range &&
	git rev-list ^tagA master >caret &&
	test_cmp range caret
	)
'

test_expect_success '--count with range through merge' '
	(
	cd merge-repo &&
	count=$(git rev-list --count tagA..master) &&
	git rev-list tagA..master >actual &&
	lines=$(wc -l <actual | tr -d " ") &&
	test "$count" = "$lines"
	)
'

test_expect_success 'rev-list with two branch tips deduplicates' '
	(
	cd merge-repo &&
	git rev-list master octopus >combined &&
	sort -u combined >unique &&
	combined_c=$(wc -l <combined | tr -d " ") &&
	unique_c=$(wc -l <unique | tr -d " ") &&
	test "$combined_c" = "$unique_c"
	)
'

test_done
