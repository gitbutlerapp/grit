#!/bin/sh
#
# Tests for rev-list with branch hierarchies and merge commits

test_description='rev-list with branch hierarchies, merge commits, and complex graphs'

. ./test-lib.sh

GIT_COMMITTER_EMAIL=git@comm.iter.xz
GIT_COMMITTER_NAME='C O Mmiter'
GIT_AUTHOR_NAME='A U Thor'
GIT_AUTHOR_EMAIL=git@au.thor.xz
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=$(PATH="/usr/bin:/usr/local/bin:$PATH" command -v git)

#
# Build a complex DAG:
#
#   A---B---C---G (main)
#        \     /
#         D---E---F (topic)
#
test_expect_success 'setup complex history with merge' '
	git init -b main . &&
	echo a >file &&
	git add file &&
	test_tick &&
	git commit -m "A" &&
	git tag A &&

	echo b >>file &&
	git add file &&
	test_tick &&
	git commit -m "B" &&
	git tag B &&

	echo c >>file &&
	git add file &&
	test_tick &&
	git commit -m "C" &&
	git tag C &&

	git checkout -b topic B &&
	echo d >topic-file &&
	git add topic-file &&
	test_tick &&
	git commit -m "D" &&
	git tag D &&

	echo e >>topic-file &&
	git add topic-file &&
	test_tick &&
	git commit -m "E" &&
	git tag E &&

	echo f >>topic-file &&
	git add topic-file &&
	test_tick &&
	git commit -m "F" &&
	git tag F &&

	git checkout main &&
	$REAL_GIT merge topic -m "G: merge topic" &&
	git tag G
'

test_expect_success 'rev-list lists all commits from merge' '
	git rev-list G >actual &&
	# G, C, F, E, D, B, A = 7
	test_line_count = 7 actual
'

test_expect_success 'rev-list --first-parent follows main line' '
	git rev-list --first-parent G >actual &&
	# G -> C -> B -> A = 4
	test_line_count = 4 actual
'

test_expect_success 'rev-list --first-parent skips topic commits' '
	git rev-list --first-parent G >actual &&
	D_SHA=$(git rev-parse D) &&
	E_SHA=$(git rev-parse E) &&
	F_SHA=$(git rev-parse F) &&
	! grep "$D_SHA" actual &&
	! grep "$E_SHA" actual &&
	! grep "$F_SHA" actual
'

test_expect_success 'rev-list range excludes reachable from base' '
	git rev-list B..G >actual &&
	# Should include: G, C, F, E, D (not B, not A)
	test_line_count = 5 actual
'

test_expect_success 'rev-list range C..G gets topic commits and merge' '
	git rev-list C..G >actual &&
	# G, F, E, D
	test_line_count = 4 actual
'

test_expect_success 'rev-list with exclusion ^C gives topic + merge' '
	git rev-list G ^C >actual &&
	test_line_count = 4 actual
'

test_expect_success 'rev-list --topo-order parents before children' '
	git rev-list --topo-order G >actual &&
	test_line_count = 7 actual
'

test_expect_success 'rev-list --date-order by commit timestamp' '
	git rev-list --date-order G >actual &&
	test_line_count = 7 actual
'

test_expect_success 'rev-list --reverse starts with root' '
	git rev-list --reverse G >actual &&
	head -1 actual >first &&
	A_SHA=$(git rev-parse A) &&
	echo "$A_SHA" >expect &&
	test_cmp expect first
'

test_expect_success 'rev-list --count counts all commits' '
	git rev-list --count G >actual &&
	test $(cat actual) = 7
'

test_expect_success 'rev-list --count with range' '
	git rev-list --count B..G >actual &&
	test $(cat actual) = 5
'

test_expect_success 'rev-list --parents shows merge parents' '
	git rev-list --parents G -1 >actual &&
	# G has 2 parents (C and F)
	line=$(cat actual) &&
	set -- $line &&
	test $# -eq 3
'

test_expect_success 'rev-list --parents shows regular commit parent' '
	git rev-list --parents D -1 >actual &&
	line=$(cat actual) &&
	set -- $line &&
	test $# -eq 2
'

# Build deeper hierarchy:
#   G---H---I (main)
#        \
#         J---K (feature)
test_expect_success 'setup second branch from merge' '
	git checkout main &&
	echo h >file2 &&
	git add file2 &&
	test_tick &&
	git commit -m "H" &&
	git tag H &&

	echo i >>file2 &&
	git add file2 &&
	test_tick &&
	git commit -m "I" &&
	git tag I &&

	git checkout -b feature H &&
	echo j >feature-file &&
	git add feature-file &&
	test_tick &&
	git commit -m "J" &&
	git tag J &&

	echo k >>feature-file &&
	git add feature-file &&
	test_tick &&
	git commit -m "K" &&
	git tag K
'

test_expect_success 'rev-list --all lists all reachable commits' '
	git rev-list --all >actual &&
	# A B C D E F G H I J K = 11
	test_line_count = 11 actual
'

test_expect_success 'rev-list H..K gives only feature commits' '
	git rev-list H..K >actual &&
	test_line_count = 2 actual
'

test_expect_success 'rev-list --first-parent I gives main chain' '
	git rev-list --first-parent I >actual &&
	# I -> H -> G -> C -> B -> A = 6
	test_line_count = 6 actual
'

test_expect_success 'rev-list I shows all ancestors including topic' '
	git rev-list I >actual &&
	# I H G C F E D B A = 9
	test_line_count = 9 actual
'

test_expect_success 'rev-list with multiple excludes' '
	git rev-list K ^I >actual &&
	# Only J and K
	test_line_count = 2 actual
'

test_expect_success 'rev-list --max-count=3 limits output' '
	git rev-list --max-count=3 I >actual &&
	test_line_count = 3 actual
'

test_expect_success 'rev-list --skip=2 from I' '
	git rev-list --skip=2 I >actual &&
	git rev-list I >all &&
	total=$(wc -l <all | tr -d " ") &&
	expected=$(( total - 2 )) &&
	test_line_count = $expected actual
'

test_expect_success 'rev-list --skip and --max-count combined' '
	git rev-list --skip=2 --max-count=3 I >actual &&
	test_line_count = 3 actual
'

# Octopus merge (3 parents)
test_expect_success 'setup octopus merge' '
	git checkout main &&
	$REAL_GIT merge feature -m "L: octopus" &&
	git tag L
'

test_expect_success 'rev-list through octopus lists all' '
	git rev-list L >actual &&
	# All 12 commits
	test_line_count = 12 actual
'

test_expect_success 'rev-list --first-parent through octopus' '
	git rev-list --first-parent L >actual &&
	# L -> I -> H -> G -> C -> B -> A = 7
	test_line_count = 7 actual
'

test_expect_success 'rev-list --parents for octopus merge shows both parents' '
	git rev-list --parents L -1 >actual &&
	line=$(cat actual) &&
	set -- $line &&
	# octopus: commit + 2 parents
	test $# -eq 3
'

test_expect_success 'rev-list with --topo-order and range' '
	git rev-list --topo-order G..L >actual &&
	# H, I, J, K, L = 5
	test_line_count = 5 actual
'

test_done
