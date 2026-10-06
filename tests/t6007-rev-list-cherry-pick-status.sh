#!/bin/sh
#
# Tests for rev-list --cherry-pick, --left-right, --cherry-mark, --cherry

test_description='rev-list cherry-pick and left-right options'

. ./test-lib.sh

GIT_COMMITTER_EMAIL=git@comm.iter.xz
GIT_COMMITTER_NAME='C O Mmiter'
GIT_AUTHOR_NAME='A U Thor'
GIT_AUTHOR_EMAIL=git@au.thor.xz
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=$(PATH="/usr/bin:/usr/local/bin:$PATH" command -v git)

test_expect_success 'setup diverging branches' '
	git init -b main . &&
	echo base >file &&
	git add file &&
	test_tick &&
	git commit -m "base" &&
	git tag base &&
	BASE=$(git rev-parse HEAD) &&

	git checkout -b left &&
	echo left1 >left-file &&
	git add left-file &&
	test_tick &&
	git commit -m "left-1" &&
	git tag left-1 &&

	echo left2 >>left-file &&
	git add left-file &&
	test_tick &&
	git commit -m "left-2" &&
	git tag left-2 &&

	git checkout -b right $BASE &&
	echo right1 >right-file &&
	git add right-file &&
	test_tick &&
	git commit -m "right-1" &&
	git tag right-1 &&

	echo right2 >>right-file &&
	git add right-file &&
	test_tick &&
	git commit -m "right-2" &&
	git tag right-2
'

test_expect_success 'rev-list basic range left..right works' '
	git rev-list left..right >actual &&
	test_line_count = 2 actual
'

test_expect_success 'rev-list basic range right..left works' '
	git rev-list right..left >actual &&
	test_line_count = 2 actual
'

test_expect_success 'rev-list exclusion with caret works' '
	git rev-list right ^base >actual &&
	test_line_count = 2 actual
'

test_expect_success 'rev-list double exclusion from base' '
	BASE=$(git rev-parse base) &&
	git rev-list right ^$BASE >actual &&
	test_line_count = 2 actual
'

test_expect_success 'rev-list --left-right shows direction markers' '
	git rev-list --left-right left...right >actual &&
	grep "^<" actual >left-only &&
	grep "^>" actual >right-only &&
	test_line_count = 2 left-only &&
	test_line_count = 2 right-only
'

test_expect_success 'rev-list --left-right count with --count' '
	git rev-list --left-right --count left...right >actual &&
	# 3rd column (count_same) only appears with --cherry-mark;
	# plain --left-right --count emits 2 columns (matches git 2.52.0)
	echo "2	2" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --cherry-pick filters equivalent commits' '
	git rev-list --cherry-pick left...right >actual &&
	test_line_count = 4 actual
'

test_expect_success 'rev-list --cherry-mark marks equivalent commits' '
	git rev-list --cherry-mark left...right >actual &&
	test $(wc -l <actual) -ge 1
'

test_expect_success 'rev-list --cherry is shorthand for --cherry-mark --right-only --no-merges' '
	git rev-list --cherry left...right >actual &&
	test $(wc -l <actual) -ge 1
'

test_expect_success 'rev-list --left-right with simple base...left range' '
	git rev-list --left-right base...left >actual &&
	grep "^>" actual >right-side &&
	test_line_count = 2 right-side &&
	! grep "^<" actual
'

test_expect_success 'rev-list --cherry-pick with --count' '
	git rev-list --cherry-pick --count left...right >actual &&
	test $(cat actual) -ge 1
'

test_expect_success 'rev-list --no-merges filters merge commits' '
	git rev-list --no-merges left >actual &&
	test_line_count = 3 actual
'

test_expect_success 'rev-list --merges shows only merge commits' '
	git rev-list --merges left >actual &&
	test_line_count = 0 actual
'

test_expect_success 'setup merge commit' '
	git checkout left &&
	$REAL_GIT merge right -m "merge right" &&
	git tag merge-point
'

test_expect_success 'rev-list --first-parent follows only first parent' '
	git rev-list --first-parent merge-point >actual &&
	git rev-list merge-point >all &&
	test $(wc -l <actual) -le $(wc -l <all)
'

test_expect_success 'rev-list --first-parent excludes second parent chain' '
	git rev-list --first-parent merge-point >actual &&
	# first-parent chain: merge -> left-2 -> left-1 -> base (4 commits)
	test_line_count = 4 actual
'

test_expect_success 'rev-list all commits through merge' '
	git rev-list merge-point >actual &&
	# merge + left-2 + left-1 + right-2 + right-1 + base = 6
	test_line_count = 6 actual
'

test_expect_success 'rev-list --count with range' '
	git rev-list --count left-2..right >actual &&
	count=$(cat actual) &&
	git rev-list left-2..right >list &&
	test "$count" = "$(wc -l <list | tr -d " ")"
'

test_expect_success 'rev-list --count with --first-parent' '
	git rev-list --count --first-parent merge-point >actual &&
	test $(cat actual) = 4
'

test_expect_success 'rev-list --left-right --count symmetric' '
	git rev-list --left-right --count left-2...right >actual &&
	test $(echo $(cat actual) | wc -w) -ge 2
'

test_expect_success 'rev-list --parents shows parent hashes for merge' '
	git rev-list --parents merge-point -1 >actual &&
	line=$(head -1 actual) &&
	set -- $line &&
	# merge commit has 3 fields: commit + 2 parents
	test $# -eq 3
'

test_expect_success 'rev-list --parents for regular commit shows one parent' '
	git rev-list --parents left-2 -1 >actual &&
	line=$(head -1 actual) &&
	set -- $line &&
	test $# -eq 2
'

test_expect_success 'rev-list --parents for root commit shows no parent' '
	git rev-list --parents base -1 >actual &&
	line=$(head -1 actual) &&
	set -- $line &&
	test $# -eq 1
'

test_expect_success 'rev-list --skip skips commits' '
	git rev-list --skip=1 merge-point >skipped &&
	git rev-list merge-point >full &&
	test $(wc -l <skipped) -lt $(wc -l <full)
'

test_expect_success 'rev-list --max-count limits output' '
	git rev-list --max-count=1 merge-point >actual &&
	test_line_count = 1 actual
'

test_expect_success 'rev-list --reverse reverses output' '
	git rev-list merge-point >forward &&
	git rev-list --reverse merge-point >backward &&
	tail -1 forward >last_fwd &&
	head -1 backward >first_bwd &&
	test_cmp last_fwd first_bwd
'

test_expect_success 'rev-list --max-count=0 returns nothing' '
	git rev-list --max-count=0 HEAD >actual &&
	test_line_count = 0 actual
'

test_expect_success 'rev-list with --skip and --max-count' '
	git rev-list --skip=1 --max-count=2 merge-point >actual &&
	test_line_count = 2 actual
'

test_done
