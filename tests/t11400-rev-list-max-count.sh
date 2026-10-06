#!/bin/sh
# Tests for grit rev-list --max-count, --count, --skip, --reverse.

test_description='grit rev-list: --max-count, --count, --skip, --reverse'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with 10 commits' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	for i in 1 2 3 4 5 6 7 8 9 10; do
		echo "commit $i" >"file$i.txt" &&
		"$REAL_GIT" add "file$i.txt" &&
		"$REAL_GIT" commit -m "commit $i" || return 1
	done
	)
'

###########################################################################
# Section 2: Basic rev-list
###########################################################################

test_expect_success 'rev-list HEAD lists all commits' '
	(
	cd repo &&
	git rev-list HEAD >output &&
	test $(wc -l <output) -eq 10
	)
'

test_expect_success 'rev-list HEAD outputs only SHA-1 hashes' '
	(
	cd repo &&
	git rev-list HEAD >output &&
	while read hash; do
		echo "$hash" | grep -qE "^[0-9a-f]{40}$" || return 1
	done <output
	)
'

test_expect_success 'rev-list HEAD contains HEAD commit' '
	(
	cd repo &&
	git rev-list HEAD >output &&
	head_hash=$(git rev-parse HEAD) &&
	grep -q "$head_hash" output
	)
'

###########################################################################
# Section 3: --max-count
###########################################################################

test_expect_success 'rev-list --max-count=1 returns one commit' '
	(
	cd repo &&
	git rev-list --max-count=1 HEAD >output &&
	test $(wc -l <output) -eq 1
	)
'

test_expect_success 'rev-list --max-count=5 returns five commits' '
	(
	cd repo &&
	git rev-list --max-count=5 HEAD >output &&
	test $(wc -l <output) -eq 5
	)
'

test_expect_success 'rev-list --max-count=0 returns no commits' '
	(
	cd repo &&
	git rev-list --max-count=0 HEAD >output &&
	test $(wc -l <output) -eq 0
	)
'

test_expect_success 'rev-list --max-count exceeding total returns all' '
	(
	cd repo &&
	git rev-list --max-count=100 HEAD >output &&
	test $(wc -l <output) -eq 10
	)
'

test_expect_success 'rev-list --max-count=1 returns exactly one hash' '
	(
	cd repo &&
	git rev-list --max-count=1 HEAD >output &&
	test $(wc -l <output) -eq 1 &&
	grep -qE "^[0-9a-f]{40}$" output
	)
'

test_expect_success 'rev-list --max-count=3 returns 3 most recent' '
	(
	cd repo &&
	git rev-list --max-count=3 HEAD >output &&
	git rev-list HEAD >all &&
	head -3 all >expected &&
	test_cmp expected output
	)
'

test_expect_success 'rev-list --max-count=4 returns 4 commits' '
	(
	cd repo &&
	git rev-list --max-count=4 HEAD >grit_out &&
	test $(wc -l <grit_out) -eq 4
	)
'

###########################################################################
# Section 4: --count
###########################################################################

test_expect_success 'rev-list --count HEAD returns total count' '
	(
	cd repo &&
	git rev-list --count HEAD >output &&
	test "$(cat output)" = "10"
	)
'

test_expect_success 'rev-list --count with --max-count' '
	(
	cd repo &&
	git rev-list --count --max-count=5 HEAD >output &&
	test "$(cat output)" = "5"
	)
'

test_expect_success 'rev-list --count matches real git' '
	(
	cd repo &&
	git rev-list --count HEAD >grit_out &&
	"$REAL_GIT" rev-list --count HEAD >git_out &&
	test_cmp grit_out git_out
	)
'

###########################################################################
# Section 5: --skip
###########################################################################

test_expect_success 'rev-list --skip=0 returns all' '
	(
	cd repo &&
	git rev-list --skip=0 HEAD >output &&
	test $(wc -l <output) -eq 10
	)
'

test_expect_success 'rev-list --skip=3 skips first 3' '
	(
	cd repo &&
	git rev-list --skip=3 HEAD >output &&
	test $(wc -l <output) -eq 7
	)
'

test_expect_success 'rev-list --skip=10 returns nothing' '
	(
	cd repo &&
	git rev-list --skip=10 HEAD >output &&
	test $(wc -l <output) -eq 0
	)
'

test_expect_success 'rev-list --skip=5 starts from 6th commit' '
	(
	cd repo &&
	git rev-list HEAD >all &&
	git rev-list --skip=5 HEAD >skipped &&
	tail -5 all >expected &&
	test_cmp expected skipped
	)
'

test_expect_success 'rev-list --skip with --max-count paginates' '
	(
	cd repo &&
	git rev-list --skip=2 --max-count=3 HEAD >output &&
	test $(wc -l <output) -eq 3
	)
'

test_expect_success 'rev-list --skip=2 --max-count=3 returns 3 commits' '
	(
	cd repo &&
	git rev-list --skip=2 --max-count=3 HEAD >grit_out &&
	test $(wc -l <grit_out) -eq 3
	)
'

test_expect_success 'rev-list --skip exceeding total returns empty' '
	(
	cd repo &&
	git rev-list --skip=100 HEAD >output &&
	test_must_be_empty output
	)
'

###########################################################################
# Section 6: --reverse
###########################################################################

test_expect_success 'rev-list --reverse reverses output' '
	(
	cd repo &&
	git rev-list HEAD >forward &&
	git rev-list --reverse HEAD >reversed &&
	last_forward=$(tail -1 forward) &&
	first_reversed=$(head -1 reversed) &&
	test "$last_forward" = "$first_reversed"
	)
'

test_expect_success 'rev-list --reverse last entry matches forward first entry' '
	(
	cd repo &&
	git rev-list HEAD >forward &&
	git rev-list --reverse HEAD >reversed &&
	first_fwd=$(head -1 forward) &&
	last_rev=$(tail -1 reversed) &&
	test "$first_fwd" = "$last_rev"
	)
'

test_expect_success 'rev-list --reverse with --max-count' '
	(
	cd repo &&
	git rev-list --reverse --max-count=3 HEAD >output &&
	test $(wc -l <output) -eq 3
	)
'

###########################################################################
# Section 7: Branch-specific rev-list
###########################################################################

test_expect_success 'setup: create branch with extra commits' '
	(
	cd repo &&
	"$REAL_GIT" checkout -b side &&
	echo "side1" >side1.txt &&
	"$REAL_GIT" add side1.txt &&
	"$REAL_GIT" commit -m "side commit 1" &&
	echo "side2" >side2.txt &&
	"$REAL_GIT" add side2.txt &&
	"$REAL_GIT" commit -m "side commit 2" &&
	"$REAL_GIT" checkout main
	)
'

test_expect_success 'rev-list side has more commits than main' '
	(
	cd repo &&
	git rev-list --count main >main_count &&
	git rev-list --count side >side_count &&
	test $(cat side_count) -gt $(cat main_count)
	)
'

test_expect_success 'rev-list --max-count on branch' '
	(
	cd repo &&
	git rev-list --max-count=2 side >output &&
	test $(wc -l <output) -eq 2
	)
'

test_expect_success 'rev-list --count side matches real git' '
	(
	cd repo &&
	git rev-list --count side >grit_out &&
	"$REAL_GIT" rev-list --count side >git_out &&
	test_cmp grit_out git_out
	)
'

###########################################################################
# Section 8: Edge cases
###########################################################################

test_expect_success 'rev-list on single-commit repo' '
	(
	"$REAL_GIT" init single &&
	cd single &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "only" >only.txt &&
	"$REAL_GIT" add only.txt &&
	"$REAL_GIT" commit -m "only" &&
	git rev-list HEAD >output &&
	test $(wc -l <output) -eq 1
	)
'

test_expect_success 'rev-list --max-count=1 on single-commit repo' '
	(
	cd single &&
	git rev-list --max-count=1 HEAD >output &&
	test $(wc -l <output) -eq 1
	)
'

test_expect_success 'rev-list --count on single-commit repo' '
	(
	cd single &&
	git rev-list --count HEAD >output &&
	test "$(cat output)" = "1"
	)
'

test_expect_success 'rev-list HEAD output is deterministic' '
	(
	cd repo &&
	git rev-list HEAD >run1 &&
	git rev-list HEAD >run2 &&
	test_cmp run1 run2
	)
'

test_expect_success 'rev-list full output contains same hashes as real git' '
	(
	cd repo &&
	git rev-list HEAD >grit_out &&
	"$REAL_GIT" rev-list HEAD >git_out &&
	sort grit_out >grit_sorted &&
	sort git_out >git_sorted &&
	test_cmp grit_sorted git_sorted
	)
'

test_done
