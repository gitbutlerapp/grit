#!/bin/sh
#
# Tests for 'grit for-each-ref' — --points-at, --merged, --count,
# pattern filtering, sorting, and format atoms.

test_description='grit for-each-ref filtering: points-at, merged, count, patterns'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

# ---------------------------------------------------------------------------
# Setup: linear history with branches and tags at various points
#
#   A --- B --- C --- D  (master)
#         |           |
#         v1.0        v2.0, release
#         feature
# ---------------------------------------------------------------------------
test_expect_success 'setup: repo with branches and tags at different points' '
	(
	$REAL_GIT init --initial-branch=master repo &&
	cd repo &&
	$REAL_GIT config user.name "Test User" &&
	$REAL_GIT config user.email "test@example.com" &&
	echo a >file &&
	$REAL_GIT add file &&
	test_tick &&
	$REAL_GIT commit -m "commit A" &&
	$REAL_GIT tag tagA &&
	echo b >file &&
	$REAL_GIT add file &&
	test_tick &&
	$REAL_GIT commit -m "commit B" &&
	$REAL_GIT tag v1.0 &&
	$REAL_GIT tag -a v1.0-annotated -m "version 1.0" &&
	$REAL_GIT branch feature &&
	echo c >file &&
	$REAL_GIT add file &&
	test_tick &&
	$REAL_GIT commit -m "commit C" &&
	echo d >file &&
	$REAL_GIT add file &&
	test_tick &&
	$REAL_GIT commit -m "commit D" &&
	$REAL_GIT tag v2.0 &&
	$REAL_GIT tag -a v2.0-annotated -m "version 2.0" &&
	$REAL_GIT branch release
	)
'

# ---------------------------------------------------------------------------
# --points-at HEAD (only refs pointing exactly at HEAD)
# ---------------------------------------------------------------------------
test_expect_success 'for-each-ref --points-at HEAD lists master' '
	(
	cd repo &&
	grit for-each-ref --points-at HEAD --format="%(refname)" >actual &&
	grep "refs/heads/master" actual
	)
'

test_expect_success 'for-each-ref --points-at HEAD lists release' '
	(
	cd repo &&
	grit for-each-ref --points-at HEAD --format="%(refname)" >actual &&
	grep "refs/heads/release" actual
	)
'

test_expect_success 'for-each-ref --points-at HEAD lists v2.0' '
	(
	cd repo &&
	grit for-each-ref --points-at HEAD --format="%(refname)" >actual &&
	grep "refs/tags/v2.0" actual
	)
'

test_expect_success 'for-each-ref --points-at HEAD excludes feature' '
	(
	cd repo &&
	grit for-each-ref --points-at HEAD --format="%(refname)" >actual &&
	! grep "refs/heads/feature" actual
	)
'

test_expect_success 'for-each-ref --points-at HEAD excludes v1.0' '
	(
	cd repo &&
	grit for-each-ref --points-at HEAD --format="%(refname)" >actual &&
	! grep "refs/tags/v1.0$" actual
	)
'

test_expect_success 'for-each-ref --points-at HEAD excludes tagA' '
	(
	cd repo &&
	grit for-each-ref --points-at HEAD --format="%(refname)" >actual &&
	! grep "refs/tags/tagA" actual
	)
'

test_expect_success 'for-each-ref --points-at HEAD matches real git' '
	(
	cd repo &&
	grit for-each-ref --points-at HEAD --format="%(refname)" | sort >actual &&
	$REAL_GIT for-each-ref --points-at HEAD --format="%(refname)" | sort >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# --points-at with tag ref
# ---------------------------------------------------------------------------
test_expect_success 'for-each-ref --points-at v1.0 lists feature and v1.0' '
	(
	cd repo &&
	sha=$($REAL_GIT rev-parse v1.0) &&
	grit for-each-ref --points-at "$sha" --format="%(refname)" >actual &&
	grep "refs/heads/feature" actual &&
	grep "refs/tags/v1.0$" actual
	)
'

test_expect_success 'for-each-ref --points-at v1.0 excludes master' '
	(
	cd repo &&
	sha=$($REAL_GIT rev-parse v1.0) &&
	grit for-each-ref --points-at "$sha" --format="%(refname)" >actual &&
	! grep "refs/heads/master" actual
	)
'

test_expect_success 'for-each-ref --points-at v1.0 SHA matches real git' '
	(
	cd repo &&
	sha=$($REAL_GIT rev-parse v1.0) &&
	grit for-each-ref --points-at "$sha" --format="%(refname)" | sort >actual &&
	$REAL_GIT for-each-ref --points-at "$sha" --format="%(refname)" | sort >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# --merged
# ---------------------------------------------------------------------------
test_expect_success 'for-each-ref --merged=master lists all refs reachable from master' '
	(
	cd repo &&
	grit for-each-ref --merged=master --format="%(refname)" >actual &&
	grep "refs/heads/master" actual &&
	grep "refs/heads/feature" actual &&
	grep "refs/tags/tagA" actual &&
	grep "refs/tags/v1.0$" actual
	)
'

test_expect_success 'for-each-ref --merged=feature excludes master' '
	(
	cd repo &&
	grit for-each-ref --merged=feature --format="%(refname)" >actual &&
	! grep "refs/heads/master" actual &&
	! grep "refs/heads/release" actual
	)
'

test_expect_success 'for-each-ref --merged=feature includes tagA and v1.0' '
	(
	cd repo &&
	grit for-each-ref --merged=feature --format="%(refname)" >actual &&
	grep "refs/tags/tagA" actual &&
	grep "refs/tags/v1.0$" actual
	)
'

test_expect_success 'for-each-ref --merged=master matches real git' '
	(
	cd repo &&
	grit for-each-ref --merged=master --format="%(refname)" | sort >actual &&
	$REAL_GIT for-each-ref --merged=master --format="%(refname)" | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref --merged=feature matches real git' '
	(
	cd repo &&
	grit for-each-ref --merged=feature --format="%(refname)" | sort >actual &&
	$REAL_GIT for-each-ref --merged=feature --format="%(refname)" | sort >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# --count
# ---------------------------------------------------------------------------
test_expect_success 'for-each-ref --count=1 shows exactly 1 ref' '
	(
	cd repo &&
	grit for-each-ref --count=1 --format="%(refname)" >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'for-each-ref --count=3 shows exactly 3 refs' '
	(
	cd repo &&
	grit for-each-ref --count=3 --format="%(refname)" >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'for-each-ref --count=100 shows all refs (count > total)' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" >all &&
	grit for-each-ref --count=100 --format="%(refname)" >actual &&
	test_cmp all actual
	)
'

# ---------------------------------------------------------------------------
# Pattern filtering
# ---------------------------------------------------------------------------
test_expect_success 'for-each-ref refs/tags lists only tags' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/tags >actual &&
	grep "refs/tags/" actual &&
	! grep "refs/heads/" actual
	)
'

test_expect_success 'for-each-ref refs/heads lists only branches' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads >actual &&
	grep "refs/heads/" actual &&
	! grep "refs/tags/" actual
	)
'

test_expect_success 'for-each-ref refs/tags matches real git' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/tags | sort >actual &&
	$REAL_GIT for-each-ref --format="%(refname)" refs/tags | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref refs/heads matches real git' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads | sort >actual &&
	$REAL_GIT for-each-ref --format="%(refname)" refs/heads | sort >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# Sorting combined with --points-at
# ---------------------------------------------------------------------------
test_expect_success 'for-each-ref --sort=-refname reverses order' '
	(
	cd repo &&
	grit for-each-ref --sort=-refname --format="%(refname)" >actual &&
	sort -r actual >expected_reverse &&
	test_cmp expected_reverse actual
	)
'

test_expect_success 'for-each-ref --sort=objecttype groups by type' '
	(
	cd repo &&
	grit for-each-ref --sort=objecttype --format="%(objecttype)" >actual &&
	sort actual >sorted &&
	test_cmp sorted actual
	)
'

test_done
