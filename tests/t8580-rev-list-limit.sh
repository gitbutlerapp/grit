#!/bin/sh
# Tests for rev-list -n, --max-count, --skip combinations.

test_description='rev-list -n, --max-count, --skip limit combinations'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup — 10 linear commits
###########################################################################

test_expect_success 'setup repository with 10 commits' '
	(
	grit init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&

	for i in 1 2 3 4 5 6 7 8 9 10; do
		echo "file $i" >file$i.txt &&
		grit add file$i.txt &&
		grit commit -m "commit $i" || return 1
	done &&

	grit rev-list HEAD >all_commits &&
	test $(wc -l <all_commits | tr -d " ") -eq 10
	)
'

###########################################################################
# Section 1: --max-count / -n basics
###########################################################################

test_expect_success 'rev-list --max-count=1 returns exactly one commit' '
	(
	cd repo &&
	grit rev-list --max-count=1 HEAD >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'rev-list -n 1 is alias for --max-count=1' '
	(
	cd repo &&
	grit rev-list -n 1 HEAD >actual &&
	grit rev-list --max-count=1 HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-list --max-count=3 returns 3 commits' '
	(
	cd repo &&
	grit rev-list --max-count=3 HEAD >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'rev-list -n 5 returns 5 commits' '
	(
	cd repo &&
	grit rev-list -n 5 HEAD >actual &&
	test_line_count = 5 actual
	)
'

test_expect_success 'rev-list --max-count=10 returns all 10' '
	(
	cd repo &&
	grit rev-list --max-count=10 HEAD >actual &&
	test_line_count = 10 actual
	)
'

test_expect_success 'rev-list --max-count=20 returns only 10 (all available)' '
	(
	cd repo &&
	grit rev-list --max-count=20 HEAD >actual &&
	test_line_count = 10 actual
	)
'

test_expect_success 'rev-list --max-count=0 returns nothing' '
	(
	cd repo &&
	grit rev-list --max-count=0 HEAD >actual &&
	test_line_count = 0 actual
	)
'

test_expect_success 'rev-list -n 1 HEAD returns exactly one commit' '
	(
	cd repo &&
	grit rev-list -n 1 HEAD >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'rev-list -n 2 returns exactly 2 commits' '
	(
	cd repo &&
	grit rev-list -n 2 HEAD >actual &&
	test_line_count = 2 actual
	)
'

###########################################################################
# Section 2: --skip
###########################################################################

test_expect_success 'rev-list --skip=0 returns all commits' '
	(
	cd repo &&
	grit rev-list --skip=0 HEAD >actual &&
	test_line_count = 10 actual
	)
'

test_expect_success 'rev-list --skip=1 returns 9 commits' '
	(
	cd repo &&
	grit rev-list --skip=1 HEAD >actual &&
	test_line_count = 9 actual
	)
'

test_expect_success 'rev-list --skip=5 returns last 5 commits' '
	(
	cd repo &&
	grit rev-list --skip=5 HEAD >actual &&
	test_line_count = 5 actual
	)
'

test_expect_success 'rev-list --skip=9 returns exactly 1 commit' '
	(
	cd repo &&
	grit rev-list --skip=9 HEAD >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'rev-list --skip=10 returns nothing' '
	(
	cd repo &&
	grit rev-list --skip=10 HEAD >actual &&
	test_line_count = 0 actual
	)
'

test_expect_success 'rev-list --skip=100 returns nothing (skip beyond total)' '
	(
	cd repo &&
	grit rev-list --skip=100 HEAD >actual &&
	test_line_count = 0 actual
	)
'

###########################################################################
# Section 3: --skip + --max-count combined
###########################################################################

test_expect_success 'rev-list --skip=1 --max-count=1 returns exactly 1 commit' '
	(
	cd repo &&
	grit rev-list --skip=1 --max-count=1 HEAD >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'rev-list --skip=2 --max-count=3 returns exactly 3 commits' '
	(
	cd repo &&
	grit rev-list --skip=2 --max-count=3 HEAD >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'rev-list --skip=8 --max-count=5 returns only last 2' '
	(
	cd repo &&
	grit rev-list --skip=8 --max-count=5 HEAD >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'rev-list --skip=0 --max-count=1 equals -n 1' '
	(
	cd repo &&
	grit rev-list --skip=0 --max-count=1 HEAD >actual &&
	grit rev-list -n 1 HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-list -n 3 --skip=5 returns exactly 3 commits' '
	(
	cd repo &&
	grit rev-list -n 3 --skip=5 HEAD >actual &&
	test_line_count = 3 actual
	)
'

###########################################################################
# Section 4: With range notation
###########################################################################

test_expect_success 'setup: record commits for range tests' '
	(
	cd repo &&
	C5=$(sed -n 6p all_commits) &&
	echo $C5 >.c5
	)
'

test_expect_success 'rev-list -n 2 with range returns limited output' '
	(
	cd repo &&
	C5=$(cat .c5) &&
	grit rev-list -n 2 HEAD ^$C5 >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'rev-list --skip=1 with range skips first in range' '
	(
	cd repo &&
	C5=$(cat .c5) &&
	grit rev-list HEAD ^$C5 >all_range &&
	grit rev-list --skip=1 HEAD ^$C5 >actual &&
	TOTAL=$(wc -l <all_range | tr -d " ") &&
	SKIPPED=$(wc -l <actual | tr -d " ") &&
	test $SKIPPED -eq $(($TOTAL - 1))
	)
'

###########################################################################
# Section 5: With branch refs
###########################################################################

test_expect_success 'create branch at commit 5' '
	(
	cd repo &&
	C5=$(cat .c5) &&
	grit branch old-branch $C5
	)
'

test_expect_success 'rev-list -n 2 old-branch returns 2 commits' '
	(
	cd repo &&
	grit rev-list -n 2 old-branch >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'rev-list --max-count=3 --skip=1 old-branch' '
	(
	cd repo &&
	grit rev-list --max-count=3 --skip=1 old-branch >actual &&
	test_line_count = 3 actual
	)
'

###########################################################################
# Section 6: --count
###########################################################################

test_expect_success 'rev-list --count HEAD returns 10' '
	(
	cd repo &&
	grit rev-list --count HEAD >actual &&
	echo "10" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-list --count --max-count=3 HEAD returns 3' '
	(
	cd repo &&
	grit rev-list --count --max-count=3 HEAD >actual &&
	echo "3" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-list --count --skip=7 HEAD returns 3' '
	(
	cd repo &&
	grit rev-list --count --skip=7 HEAD >actual &&
	echo "3" >expect &&
	test_cmp expect actual
	)
'

test_done
