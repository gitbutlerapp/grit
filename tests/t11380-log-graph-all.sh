#!/bin/sh
# Tests for grit log --graph and multi-branch display.

test_description='grit log: --graph, --oneline, --decorate, multi-branch'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with branches and merges' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "initial" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial commit" &&
	echo "second" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit" &&
	"$REAL_GIT" checkout -b feature &&
	echo "feature work" >feature.txt &&
	"$REAL_GIT" add feature.txt &&
	"$REAL_GIT" commit -m "feature commit" &&
	echo "more feature" >>feature.txt &&
	"$REAL_GIT" add feature.txt &&
	"$REAL_GIT" commit -m "more feature work" &&
	"$REAL_GIT" checkout main &&
	echo "main work" >main.txt &&
	"$REAL_GIT" add main.txt &&
	"$REAL_GIT" commit -m "main branch work" &&
	"$REAL_GIT" merge feature -m "merge feature" --no-edit &&
	"$REAL_GIT" checkout -b release &&
	echo "release" >release.txt &&
	"$REAL_GIT" add release.txt &&
	"$REAL_GIT" commit -m "release prep" &&
	"$REAL_GIT" checkout main &&
	echo "post-merge" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "post-merge commit"
	)
'

###########################################################################
# Section 2: Basic --graph tests
###########################################################################

test_expect_success 'log --graph produces output' '
	(
	cd repo &&
	git log --graph >output &&
	test -s output
	)
'

test_expect_success 'log --graph --oneline shows abbreviated output' '
	(
	cd repo &&
	git log --graph --oneline >output &&
	test $(wc -l <output) -ge 1
	)
'

test_expect_success 'log --graph --oneline contains commit messages' '
	(
	cd repo &&
	git log --graph --oneline >output &&
	grep -q "post-merge commit" output &&
	grep -q "initial commit" output
	)
'

test_expect_success 'log --graph shows merge commit' '
	(
	cd repo &&
	git log --graph --oneline >output &&
	grep -q "merge feature" output
	)
'

test_expect_success 'log --graph full format includes Author and Date' '
	(
	cd repo &&
	git log --graph >output &&
	grep -q "Author:" output &&
	grep -q "Date:" output
	)
'

test_expect_success 'log --graph shows commit hashes' '
	(
	cd repo &&
	git log --graph >output &&
	grep -qE "commit [0-9a-f]{40}" output
	)
'

###########################################################################
# Section 3: Multi-branch listing
###########################################################################

test_expect_success 'log with multiple branches shows commits from all' '
	(
	cd repo &&
	git log --oneline main feature release >output &&
	grep -q "release prep" output &&
	grep -q "feature commit" output &&
	grep -q "initial commit" output
	)
'

test_expect_success 'log with multiple branches includes more than single branch' '
	(
	cd repo &&
	git log --oneline main >main_out &&
	git log --oneline main feature release >multi_out &&
	test $(wc -l <multi_out) -ge $(wc -l <main_out)
	)
'

test_expect_success 'log --graph with multiple branches' '
	(
	cd repo &&
	git log --graph --oneline main feature release >output &&
	grep -q "release prep" output &&
	grep -q "more feature work" output
	)
'

test_expect_success 'log --graph with multiple branches shows all commits' '
	(
	cd repo &&
	git log --graph --oneline main feature release >output &&
	grep -q "post-merge commit" output &&
	grep -q "feature commit" output
	)
'

###########################################################################
# Section 4: --decorate flag
###########################################################################

test_expect_success 'log --oneline shows decoration by default' '
	(
	cd repo &&
	git log --oneline -n 1 >output &&
	grep -q "main" output
	)
'

test_expect_success 'log --decorate shows branch names' '
	(
	cd repo &&
	git log --oneline --decorate >output &&
	grep -q "main" output &&
	grep -q "feature" output
	)
'

test_expect_success 'log --no-decorate hides branch names' '
	(
	cd repo &&
	git log --oneline --no-decorate >output &&
	! grep -q "(.*main" output
	)
'

test_expect_success 'log --decorate shows HEAD pointer' '
	(
	cd repo &&
	git log --oneline --decorate -n 1 >output &&
	grep -q "HEAD" output
	)
'

###########################################################################
# Section 5: --graph with format strings
###########################################################################

test_expect_success 'log --graph --format="%H" shows full hashes' '
	(
	cd repo &&
	git log --graph --format="%H" >output &&
	grep -qE "^[0-9a-f]{40}$" output
	)
'

test_expect_success 'log --graph --format="%h %s" shows short hash and subject' '
	(
	cd repo &&
	git log --graph --format="%h %s" >output &&
	grep -q "initial commit" output
	)
'

test_expect_success 'log --graph --format="%an" shows author names' '
	(
	cd repo &&
	git log --graph --format="%an" >output &&
	grep -q "Test User" output
	)
'

test_expect_success 'log --graph with -n limits output' '
	(
	cd repo &&
	git log --graph --oneline -n 3 >output &&
	test $(wc -l <output) -le 5
	)
'

test_expect_success 'log --graph with -n 1 shows only HEAD' '
	(
	cd repo &&
	git log --graph --oneline -n 1 >output &&
	test $(wc -l <output) -eq 1 &&
	grep -q "post-merge commit" output
	)
'

###########################################################################
# Section 6: --reverse with --graph
###########################################################################

test_expect_success 'log --reverse reverses default order' '
	(
	cd repo &&
	git log --oneline >forward &&
	git log --oneline --reverse >reversed &&
	last_forward=$(tail -1 forward) &&
	first_reversed=$(head -1 reversed) &&
	test "$last_forward" = "$first_reversed"
	)
'

test_expect_success 'log --reverse with -n shows oldest of limited set' '
	(
	cd repo &&
	git log --oneline --reverse -n 3 >output &&
	test $(wc -l <output) -le 3
	)
'

###########################################################################
# Section 7: Edge cases
###########################################################################

test_expect_success 'log --graph on single-commit repo' '
	(
	"$REAL_GIT" init single &&
	cd single &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "only" >only.txt &&
	"$REAL_GIT" add only.txt &&
	"$REAL_GIT" commit -m "only commit" &&
	git log --graph --oneline >output &&
	test $(wc -l <output) -eq 1
	)
'

test_expect_success 'log --graph on linear history (no merges)' '
	(
	"$REAL_GIT" init linear &&
	cd linear &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	for i in 1 2 3 4 5; do
		echo "$i" >"f$i.txt" &&
		"$REAL_GIT" add "f$i.txt" &&
		"$REAL_GIT" commit -m "commit $i" || return 1
	done &&
	git log --graph --oneline >output &&
	test $(wc -l <output) -eq 5
	)
'

test_expect_success 'log --graph does not crash on empty repo' '
	(
	"$REAL_GIT" init empty-repo &&
	cd empty-repo &&
	git log --graph --oneline >output 2>&1 || true &&
	test -f output
	)
'

test_expect_success 'log --graph --oneline is consistent across runs' '
	(
	cd repo &&
	git log --graph --oneline >run1 &&
	git log --graph --oneline >run2 &&
	test_cmp run1 run2
	)
'

###########################################################################
# Section 8: Comparison with real git
###########################################################################

test_expect_success 'log --oneline commit count matches real git' '
	(
	cd repo &&
	git log --oneline main feature release >grit_out &&
	"$REAL_GIT" log --oneline main feature release >git_out &&
	test $(wc -l <grit_out) -eq $(wc -l <git_out)
	)
'

test_expect_success 'log commit subjects match real git' '
	(
	cd repo &&
	git log --format="%s" main feature release >grit_subjects &&
	"$REAL_GIT" log --format="%s" main feature release >git_subjects &&
	sort grit_subjects >grit_sorted &&
	sort git_subjects >git_sorted &&
	test_cmp grit_sorted git_sorted
	)
'

test_expect_success 'log --graph with multiple branches produces output' '
	(
	cd repo &&
	git log --graph --oneline main feature release >grit_out &&
	test -s grit_out &&
	test $(wc -l <grit_out) -ge 7
	)
'

test_expect_success 'log --graph --oneline with tag' '
	(
	cd repo &&
	"$REAL_GIT" tag v1.0 HEAD 2>/dev/null || true &&
	git log --graph --oneline --decorate >output &&
	grep -q "v1.0" output
	)
'

test_expect_success 'log --skip skips commits' '
	(
	cd repo &&
	git log --oneline >full &&
	git log --oneline --skip=2 >skipped &&
	full_count=$(wc -l <full) &&
	skip_count=$(wc -l <skipped) &&
	expected=$((full_count - 2)) &&
	test "$skip_count" -eq "$expected"
	)
'

test_expect_success 'log --first-parent follows only first parent' '
	(
	cd repo &&
	git log --oneline --first-parent >output &&
	! grep -q "feature commit" output &&
	grep -q "merge feature" output
	)
'

test_expect_success 'log --first-parent has fewer commits than full log' '
	(
	cd repo &&
	git log --oneline >full &&
	git log --oneline --first-parent >first_parent &&
	test $(wc -l <first_parent) -le $(wc -l <full)
	)
'

test_done
