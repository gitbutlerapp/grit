#!/bin/sh
# Tests for rev-list --first-parent and related traversal options.

test_description='rev-list --first-parent and traversal'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- setup -----------------------------------------------------------------

test_expect_success 'setup: linear history with merge' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "base" >file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "A" &&
	$REAL_GIT tag A &&
	echo "b" >b.txt &&
	$REAL_GIT add b.txt &&
	test_tick &&
	$REAL_GIT commit -m "B" &&
	$REAL_GIT tag B &&
	$REAL_GIT checkout -b side A &&
	echo "c" >c.txt &&
	$REAL_GIT add c.txt &&
	test_tick &&
	$REAL_GIT commit -m "C" &&
	$REAL_GIT tag C &&
	echo "d" >d.txt &&
	$REAL_GIT add d.txt &&
	test_tick &&
	$REAL_GIT commit -m "D" &&
	$REAL_GIT tag D &&
	$REAL_GIT checkout master &&
	$REAL_GIT merge --no-edit side &&
	$REAL_GIT tag M &&
	echo "e" >e.txt &&
	$REAL_GIT add e.txt &&
	test_tick &&
	$REAL_GIT commit -m "E" &&
	$REAL_GIT tag E
	)
'

# -- basic rev-list ---------------------------------------------------------

test_expect_success 'rev-list HEAD lists all commits (as set)' '
	(
	cd repo &&
	grit rev-list HEAD | sort >actual &&
	$REAL_GIT rev-list HEAD | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-list HEAD count matches git' '
	(
	cd repo &&
	grit rev-list HEAD >actual &&
	test $(wc -l <actual) = $($REAL_GIT rev-list --count HEAD)
	)
'

test_expect_success 'rev-list single commit' '
	(
	cd repo &&
	grit rev-list A >actual &&
	echo $(grit rev-parse A) >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-list from tag B shows 2 commits' '
	(
	cd repo &&
	grit rev-list B >actual &&
	$REAL_GIT rev-list B >expect &&
	test_cmp expect actual
	)
'

# -- --first-parent ---------------------------------------------------------

test_expect_success 'rev-list --first-parent HEAD follows main line only' '
	(
	cd repo &&
	grit rev-list --first-parent HEAD >actual &&
	$REAL_GIT rev-list --first-parent HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-list --first-parent excludes side branch commits' '
	(
	cd repo &&
	grit rev-list --first-parent HEAD >actual &&
	c_sha=$(grit rev-parse C) &&
	d_sha=$(grit rev-parse D) &&
	! grep "$c_sha" actual &&
	! grep "$d_sha" actual
	)
'

test_expect_success 'rev-list --first-parent from merge includes merge itself' '
	(
	cd repo &&
	grit rev-list --first-parent M >actual &&
	m_sha=$(grit rev-parse M) &&
	grep "$m_sha" actual
	)
'

test_expect_success 'rev-list --first-parent count is less than full count' '
	(
	cd repo &&
	grit rev-list --first-parent HEAD >fp &&
	grit rev-list HEAD >all &&
	test $(wc -l <fp) -lt $(wc -l <all)
	)
'

test_expect_success 'rev-list --first-parent HEAD starts with HEAD' '
	(
	cd repo &&
	grit rev-list --first-parent HEAD >actual &&
	head_sha=$(grit rev-parse HEAD) &&
	first=$(head -1 actual) &&
	test "$first" = "$head_sha"
	)
'

# -- --max-count / -n -------------------------------------------------------

test_expect_success 'rev-list --max-count=2 limits output' '
	(
	cd repo &&
	grit rev-list --max-count=2 HEAD >actual &&
	test $(wc -l <actual) = 2
	)
'

test_expect_success 'rev-list -n 1 shows only HEAD' '
	(
	cd repo &&
	grit rev-list -n 1 HEAD >actual &&
	grit rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-list --max-count=0 shows nothing' '
	(
	cd repo &&
	grit rev-list --max-count=0 HEAD >actual &&
	test $(wc -l <actual) = 0
	)
'

test_expect_success 'rev-list -n 3 --first-parent limits first-parent walk' '
	(
	cd repo &&
	grit rev-list -n 3 --first-parent HEAD >actual &&
	$REAL_GIT rev-list -n 3 --first-parent HEAD >expect &&
	test_cmp expect actual
	)
'

# -- exclusion (^ref / ref..ref) -------------------------------------------

test_expect_success 'rev-list with exclusion ^A shows commits after A (as set)' '
	(
	cd repo &&
	grit rev-list HEAD ^A | sort >actual &&
	$REAL_GIT rev-list HEAD ^A | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-list B..HEAD excludes B and ancestors (as set)' '
	(
	cd repo &&
	grit rev-list B..HEAD | sort >actual &&
	$REAL_GIT rev-list B..HEAD | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-list A..B shows single commit' '
	(
	cd repo &&
	grit rev-list A..B >actual &&
	$REAL_GIT rev-list A..B >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-list A..A shows nothing' '
	(
	cd repo &&
	grit rev-list A..A >actual &&
	test $(wc -l <actual) = 0
	)
'

test_expect_success 'rev-list excludes root with ^A' '
	(
	cd repo &&
	grit rev-list HEAD ^A >actual &&
	a_sha=$(grit rev-parse A) &&
	! grep "$a_sha" actual
	)
'

# -- --reverse --------------------------------------------------------------

test_expect_success 'rev-list --reverse HEAD has same set as forward' '
	(
	cd repo &&
	grit rev-list HEAD | sort >fwd &&
	grit rev-list --reverse HEAD | sort >rev &&
	test_cmp fwd rev
	)
'

test_expect_success 'rev-list --reverse first line is root commit' '
	(
	cd repo &&
	grit rev-list --reverse HEAD >actual &&
	a_sha=$(grit rev-parse A) &&
	first=$(head -1 actual) &&
	test "$first" = "$a_sha"
	)
'

test_expect_success 'rev-list --reverse last line is HEAD' '
	(
	cd repo &&
	grit rev-list --reverse HEAD >actual &&
	head_sha=$(grit rev-parse HEAD) &&
	last=$(tail -1 actual) &&
	test "$last" = "$head_sha"
	)
'

test_expect_success 'rev-list --reverse --first-parent HEAD' '
	(
	cd repo &&
	grit rev-list --reverse --first-parent HEAD >actual &&
	$REAL_GIT rev-list --reverse --first-parent HEAD >expect &&
	test_cmp expect actual
	)
'

# -- --ancestry-path --------------------------------------------------------

test_expect_success 'rev-list --ancestry-path A..HEAD (as set)' '
	(
	cd repo &&
	grit rev-list --ancestry-path A..HEAD | sort >actual &&
	$REAL_GIT rev-list --ancestry-path A..HEAD | sort >expect &&
	test_cmp expect actual
	)
'

# -- merge topology ---------------------------------------------------------

test_expect_success 'rev-list includes both parents of merge' '
	(
	cd repo &&
	grit rev-list HEAD >actual &&
	b_sha=$(grit rev-parse B) &&
	d_sha=$(grit rev-parse D) &&
	grep "$b_sha" actual &&
	grep "$d_sha" actual
	)
'

test_expect_success 'rev-list from side branch D shows C and A' '
	(
	cd repo &&
	grit rev-list D >actual &&
	$REAL_GIT rev-list D >expect &&
	test_cmp expect actual
	)
'

# -- commit limiting with tags -----------------------------------------------

test_expect_success 'rev-list C..M includes D and B and M (as set)' '
	(
	cd repo &&
	grit rev-list C..M | sort >actual &&
	$REAL_GIT rev-list C..M | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-list --first-parent --max-count=1 M shows merge' '
	(
	cd repo &&
	grit rev-list --first-parent --max-count=1 M >actual &&
	grit rev-parse M >expect &&
	test_cmp expect actual
	)
'

# -- multiple positive refs --------------------------------------------------

test_expect_success 'rev-list with two positive refs unions them (as set)' '
	(
	cd repo &&
	grit rev-list B D | sort >actual &&
	$REAL_GIT rev-list B D | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-list B D ^A excludes root (as set)' '
	(
	cd repo &&
	grit rev-list B D ^A | sort >actual &&
	$REAL_GIT rev-list B D ^A | sort >expect &&
	test_cmp expect actual
	)
'

# -- various edge cases ------------------------------------------------------

test_expect_success 'rev-list of root commit shows one line' '
	(
	cd repo &&
	grit rev-list A >actual &&
	test $(wc -l <actual) = 1
	)
'

test_expect_success 'rev-list --first-parent of non-merge is same as rev-list' '
	(
	cd repo &&
	grit rev-list --first-parent B >actual &&
	grit rev-list B >expect &&
	test_cmp expect actual
	)
'

test_done
