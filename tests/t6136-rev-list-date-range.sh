#!/bin/sh
# Tests for rev-list range exclusions, multi-ref walks, and boundary conditions.

test_description='rev-list range and exclusion patterns'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

. ./test-lib.sh

GIT_AUTHOR_NAME='A U Thor'
GIT_AUTHOR_EMAIL='author@example.com'
GIT_COMMITTER_NAME='C O Mmiter'
GIT_COMMITTER_EMAIL='committer@example.com'
export GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL

test_expect_success 'setup: linear history on master' '
	(
	git init repo &&
	cd repo &&
	test_commit A &&
	test_commit B &&
	test_commit C &&
	test_commit D &&
	test_commit E
	)
'

test_expect_success 'rev-list HEAD lists all 5 commits' '
	(
	cd repo &&
	git rev-list HEAD >actual &&
	test_line_count = 5 actual
	)
'

test_expect_success 'A..E excludes A itself' '
	(
	cd repo &&
	A=$(git rev-parse A) &&
	git rev-list A..E >actual &&
	! grep -q "$A" actual
	)
'

test_expect_success 'A..E includes B through E' '
	(
	cd repo &&
	git rev-list A..E >actual &&
	test_line_count = 4 actual
	)
'

test_expect_success 'B..D is a proper subset' '
	(
	cd repo &&
	git rev-list B..D >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'C..C is empty (same commit)' '
	(
	cd repo &&
	git rev-list C..C >actual &&
	test_must_be_empty actual
	)
'

test_expect_success '^C D gives just D' '
	(
	cd repo &&
	git rev-list ^C D >actual &&
	test_line_count = 1 actual &&
	D=$(git rev-parse D) &&
	grep -q "$D" actual
	)
'

test_expect_success 'A..B is same as ^A B' '
	(
	cd repo &&
	git rev-list A..B >range &&
	git rev-list ^A B >caret &&
	test_cmp range caret
	)
'

test_expect_success '--count matches line count for ranges' '
	(
	cd repo &&
	count=$(git rev-list --count B..E) &&
	git rev-list B..E >actual &&
	lines=$(wc -l <actual | tr -d " ") &&
	test "$count" = "$lines"
	)
'

test_expect_success 'setup: create branch from B' '
	(
	cd repo &&
	git update-ref refs/heads/side $(git rev-parse B) &&
	git checkout side &&
	test_commit S1 &&
	test_commit S2 &&
	test_commit S3 &&
	git checkout master
	)
'

test_expect_success 'rev-list side has 5 commits (A,B,S1,S2,S3)' '
	(
	cd repo &&
	git rev-list side >actual &&
	test_line_count = 5 actual
	)
'

test_expect_success 'master..side excludes shared history' '
	(
	cd repo &&
	git rev-list master..side >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'side..master excludes shared history the other way' '
	(
	cd repo &&
	git rev-list side..master >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'multiple exclusions: ^side ^A master' '
	(
	cd repo &&
	A=$(git rev-parse A) &&
	git rev-list ^side ^A master >actual &&
	# master is A-B-C-D-E, side is A-B-S1-S2-S3
	# ^A removes A; ^side removes A,B,S1,S2,S3
	# Remaining from master: C,D,E
	test_line_count = 3 actual
	)
'

test_expect_success 'rev-list with two tips merges walks' '
	(
	cd repo &&
	git rev-list master side >combined &&
	# master=5, side=5, shared=A,B => unique=8
	test_line_count = 8 combined
	)
'

test_expect_success '--count with two tips' '
	(
	cd repo &&
	count=$(git rev-list --count master side) &&
	test "$count" = "8"
	)
'

test_expect_success 'exclude multiple refs with ^' '
	(
	cd repo &&
	git rev-list ^A ^B master >actual &&
	# Should exclude A and B, leaving C,D,E
	test_line_count = 3 actual
	)
'

test_expect_success '--max-count=2 on range' '
	(
	cd repo &&
	git rev-list --max-count=2 A..E >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success '--skip=1 on range' '
	(
	cd repo &&
	git rev-list A..E >full &&
	git rev-list --skip=1 A..E >skipped &&
	full_c=$(wc -l <full | tr -d " ") &&
	skip_c=$(wc -l <skipped | tr -d " ") &&
	test "$skip_c" = "$(($full_c - 1))"
	)
'

test_expect_success '--skip and --max-count on range' '
	(
	cd repo &&
	git rev-list --skip=1 --max-count=2 A..E >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success '--max-count=0 returns empty' '
	(
	cd repo &&
	git rev-list --max-count=0 HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'rev-list with tag ref' '
	(
	cd repo &&
	git rev-list C >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'rev-list with explicit SHA' '
	(
	cd repo &&
	sha=$(git rev-parse D) &&
	git rev-list "$sha" >actual &&
	test_line_count = 4 actual
	)
'

test_expect_success '--reverse outputs commits' '
	(
	cd repo &&
	git rev-list HEAD >forward &&
	git rev-list --reverse HEAD >reversed &&
	fwd_c=$(wc -l <forward | tr -d " ") &&
	rev_c=$(wc -l <reversed | tr -d " ") &&
	test "$fwd_c" = "$rev_c"
	)
'

test_expect_success '--topo-order preserves set of commits' '
	(
	cd repo &&
	git rev-list HEAD | sort >default_sorted &&
	git rev-list --topo-order HEAD | sort >topo_sorted &&
	test_cmp default_sorted topo_sorted
	)
'

test_expect_success '--date-order preserves set of commits' '
	(
	cd repo &&
	git rev-list HEAD | sort >default_sorted &&
	git rev-list --date-order HEAD | sort >date_sorted &&
	test_cmp default_sorted date_sorted
	)
'

test_expect_success 'range from root: root is not included' '
	(
	cd repo &&
	A=$(git rev-parse A) &&
	git rev-list A..HEAD >actual &&
	! grep -q "$A" actual
	)
'

test_expect_success '--count on full walk matches actual count' '
	(
	cd repo &&
	count=$(git rev-list --count HEAD) &&
	git rev-list HEAD >actual &&
	lines=$(wc -l <actual | tr -d " ") &&
	test "$count" = "$lines"
	)
'

test_expect_success '--quiet suppresses output for ranges too' '
	(
	cd repo &&
	git rev-list --quiet A..E >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'rev-list single root commit' '
	(
	cd repo &&
	git rev-list --max-count=1 A >actual &&
	test_line_count = 1 actual &&
	A=$(git rev-parse A) &&
	grep -q "$A" actual
	)
'

test_expect_success '--first-parent on linear history is same as default' '
	(
	cd repo &&
	git rev-list HEAD >default_out &&
	git rev-list --first-parent HEAD >fp_out &&
	test_cmp default_out fp_out
	)
'

test_done
