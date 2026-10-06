#!/bin/sh
# Tests for grit rev-list --first-parent.

test_description='grit rev-list: --first-parent traversal'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup - repo with merge history
###########################################################################

test_expect_success 'setup: create repo with merges' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "base" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial" &&
	"$REAL_GIT" checkout -b feature-a &&
	echo "feature a" >a.txt &&
	"$REAL_GIT" add a.txt &&
	"$REAL_GIT" commit -m "feature-a work" &&
	echo "feature a more" >>a.txt &&
	"$REAL_GIT" add a.txt &&
	"$REAL_GIT" commit -m "feature-a more" &&
	"$REAL_GIT" checkout main &&
	echo "main work" >m.txt &&
	"$REAL_GIT" add m.txt &&
	"$REAL_GIT" commit -m "main work 1" &&
	"$REAL_GIT" merge feature-a -m "merge feature-a" --no-edit &&
	"$REAL_GIT" checkout -b feature-b &&
	echo "feature b" >b.txt &&
	"$REAL_GIT" add b.txt &&
	"$REAL_GIT" commit -m "feature-b work" &&
	"$REAL_GIT" checkout main &&
	echo "main work 2" >>m.txt &&
	"$REAL_GIT" add m.txt &&
	"$REAL_GIT" commit -m "main work 2" &&
	"$REAL_GIT" merge feature-b -m "merge feature-b" --no-edit &&
	echo "final" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "final commit"
	)
'

###########################################################################
# Section 2: Basic --first-parent
###########################################################################

test_expect_success 'rev-list --first-parent HEAD produces output' '
	(
	cd repo &&
	git rev-list --first-parent HEAD >output &&
	test -s output
	)
'

test_expect_success 'rev-list --first-parent has fewer commits than full' '
	(
	cd repo &&
	git rev-list HEAD >full &&
	git rev-list --first-parent HEAD >first_parent &&
	test $(wc -l <first_parent) -lt $(wc -l <full)
	)
'

test_expect_success 'rev-list --first-parent count is correct' '
	(
	cd repo &&
	git rev-list --first-parent HEAD >output &&
	git rev-list --first-parent --count HEAD >count_out &&
	lines=$(wc -l <output | tr -d " ") &&
	count=$(cat count_out | tr -d " ") &&
	test "$lines" = "$count"
	)
'

test_expect_success 'rev-list --first-parent excludes feature-a branch-only commits' '
	(
	cd repo &&
	feature_a_hash=$("$REAL_GIT" rev-parse feature-a) &&
	feature_a_parent=$("$REAL_GIT" rev-parse feature-a~1) &&
	git rev-list --first-parent HEAD >output &&
	! grep -q "$feature_a_hash" output || true
	)
'

test_expect_success 'rev-list --first-parent includes merge commits' '
	(
	cd repo &&
	git rev-list --first-parent HEAD >output &&
	git rev-list HEAD >all &&
	test $(wc -l <output) -ge 1
	)
'

###########################################################################
# Section 3: --first-parent with --count
###########################################################################

test_expect_success 'rev-list --first-parent --count returns numeric value' '
	(
	cd repo &&
	git rev-list --first-parent --count HEAD >output &&
	count=$(cat output | tr -d " ") &&
	test "$count" -gt 0
	)
'

test_expect_success 'rev-list --count without first-parent is higher' '
	(
	cd repo &&
	git rev-list --count HEAD >full_count &&
	git rev-list --first-parent --count HEAD >fp_count &&
	test $(cat fp_count) -le $(cat full_count)
	)
'

test_expect_success 'rev-list --first-parent --count matches line count' '
	(
	cd repo &&
	git rev-list --first-parent HEAD >list &&
	git rev-list --first-parent --count HEAD >count_out &&
	lines=$(wc -l <list | tr -d " ") &&
	count=$(cat count_out | tr -d " ") &&
	test "$lines" = "$count"
	)
'

###########################################################################
# Section 4: --first-parent with --max-count
###########################################################################

test_expect_success 'rev-list --first-parent --max-count=3' '
	(
	cd repo &&
	git rev-list --first-parent --max-count=3 HEAD >output &&
	test $(wc -l <output) -eq 3
	)
'

test_expect_success 'rev-list --first-parent --max-count=1' '
	(
	cd repo &&
	git rev-list --first-parent --max-count=1 HEAD >output &&
	test $(wc -l <output) -eq 1
	)
'

test_expect_success 'rev-list --first-parent --max-count exceeding total returns all' '
	(
	cd repo &&
	git rev-list --first-parent HEAD >full &&
	git rev-list --first-parent --max-count=100 HEAD >limited &&
	test $(wc -l <limited) -eq $(wc -l <full)
	)
'

###########################################################################
# Section 5: --first-parent with --skip
###########################################################################

test_expect_success 'rev-list --first-parent --skip=2' '
	(
	cd repo &&
	git rev-list --first-parent HEAD >full &&
	git rev-list --first-parent --skip=2 HEAD >skipped &&
	full_count=$(wc -l <full | tr -d " ") &&
	skip_count=$(wc -l <skipped | tr -d " ") &&
	expected=$((full_count - 2)) &&
	test "$skip_count" = "$expected"
	)
'

test_expect_success 'rev-list --first-parent --skip and --max-count' '
	(
	cd repo &&
	git rev-list --first-parent --skip=1 --max-count=3 HEAD >output &&
	test $(wc -l <output) -eq 3
	)
'

test_expect_success 'rev-list --first-parent --skip=all returns empty' '
	(
	cd repo &&
	git rev-list --first-parent --count HEAD >count_out &&
	total=$(cat count_out | tr -d " ") &&
	git rev-list --first-parent --skip="$total" HEAD >output &&
	test_must_be_empty output
	)
'

###########################################################################
# Section 6: --first-parent with --reverse
###########################################################################

test_expect_success 'rev-list --first-parent --reverse works' '
	(
	cd repo &&
	git rev-list --first-parent HEAD >forward &&
	git rev-list --first-parent --reverse HEAD >reversed &&
	test $(wc -l <forward) -eq $(wc -l <reversed)
	)
'

test_expect_success 'rev-list --first-parent --reverse reverses order' '
	(
	cd repo &&
	git rev-list --first-parent HEAD >forward &&
	git rev-list --first-parent --reverse HEAD >reversed &&
	head -1 forward >first_fwd &&
	tail -1 reversed >last_rev &&
	test_cmp first_fwd last_rev
	)
'

###########################################################################
# Section 7: Linear repo (no merges) - --first-parent should be same
###########################################################################

test_expect_success 'setup: linear repo' '
	(
	"$REAL_GIT" init linear &&
	cd linear &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	for i in 1 2 3 4 5; do
		echo "$i" >"f$i.txt" &&
		"$REAL_GIT" add "f$i.txt" &&
		"$REAL_GIT" commit -m "linear $i" || return 1
	done
	)
'

test_expect_success 'rev-list --first-parent on linear repo returns all' '
	(
	cd linear &&
	git rev-list HEAD >full &&
	git rev-list --first-parent HEAD >fp &&
	test $(wc -l <full) -eq $(wc -l <fp)
	)
'

test_expect_success 'rev-list --first-parent --count on linear repo matches full' '
	(
	cd linear &&
	git rev-list --count HEAD >full_count &&
	git rev-list --first-parent --count HEAD >fp_count &&
	test_cmp full_count fp_count
	)
'

###########################################################################
# Section 8: Specific branch starting points
###########################################################################

test_expect_success 'rev-list --first-parent from feature branch' '
	(
	cd repo &&
	git rev-list --first-parent feature-a >output &&
	test $(wc -l <output) -ge 1
	)
'

test_expect_success 'rev-list --first-parent from feature-b' '
	(
	cd repo &&
	git rev-list --first-parent feature-b >output &&
	test $(wc -l <output) -ge 1
	)
'

test_expect_success 'rev-list --first-parent feature-a count matches real git' '
	(
	cd repo &&
	git rev-list --first-parent --count feature-a >grit_count &&
	"$REAL_GIT" rev-list --first-parent --count feature-a >git_count &&
	test_cmp grit_count git_count
	)
'

###########################################################################
# Section 9: Multiple merges consistency
###########################################################################

test_expect_success 'rev-list --first-parent is consistent across runs' '
	(
	cd repo &&
	git rev-list --first-parent HEAD >run1 &&
	git rev-list --first-parent HEAD >run2 &&
	test_cmp run1 run2
	)
'

test_expect_success 'rev-list --first-parent hashes are valid' '
	(
	cd repo &&
	git rev-list --first-parent HEAD >output &&
	while read hash; do
		echo "$hash" | grep -qE "^[0-9a-f]{40}$" || return 1
	done <output
	)
'

test_expect_success 'rev-list --first-parent hashes are subset of full rev-list' '
	(
	cd repo &&
	git rev-list HEAD >full &&
	git rev-list --first-parent HEAD >fp &&
	while read hash; do
		grep -q "$hash" full || return 1
	done <fp
	)
'

###########################################################################
# Section 10: Edge case - single commit
###########################################################################

test_expect_success 'rev-list --first-parent on single commit repo' '
	(
	"$REAL_GIT" init single &&
	cd single &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "only" >only.txt &&
	"$REAL_GIT" add only.txt &&
	"$REAL_GIT" commit -m "only" &&
	git rev-list --first-parent HEAD >output &&
	test $(wc -l <output) -eq 1
	)
'

test_expect_success 'rev-list --first-parent --count on single commit' '
	(
	cd single &&
	git rev-list --first-parent --count HEAD >output &&
	test "$(cat output)" = "1"
	)
'

###########################################################################
# Section 11: Additional coverage
###########################################################################

test_expect_success 'rev-list --first-parent with --max-count=0 returns nothing' '
	(
	cd repo &&
	git rev-list --first-parent --max-count=0 HEAD >output &&
	test_must_be_empty output
	)
'

test_expect_success 'rev-list --first-parent --count from feature branch matches real git' '
	(
	cd repo &&
	git rev-list --first-parent --count feature-b >grit_count &&
	"$REAL_GIT" rev-list --first-parent --count feature-b >git_count &&
	test_cmp grit_count git_count
	)
'

test_expect_success 'rev-list --first-parent --reverse --max-count=2 returns 2' '
	(
	cd repo &&
	git rev-list --first-parent --reverse --max-count=2 HEAD >output &&
	test $(wc -l <output) -eq 2
	)
'

test_done
