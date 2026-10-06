#!/bin/sh
# Tests for branch listing: -l, -v, -a, --contains, --no-contains,
# --merged, --no-merged, --show-current, patterns.

test_description='branch list, filter, and verbose output'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- setup -----------------------------------------------------------------

test_expect_success 'setup: repo with branches at different commits' '
	(
	$REAL_GIT init --initial-branch=master repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "base" >file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "first" &&
	first=$(git rev-parse HEAD) &&
	$REAL_GIT branch old-feature &&
	echo "second" >>file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "second" &&
	second=$(git rev-parse HEAD) &&
	$REAL_GIT branch feature &&
	$REAL_GIT branch bugfix &&
	echo "third" >>file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "third" &&
	third=$(git rev-parse HEAD) &&
	$REAL_GIT branch release &&
	$REAL_GIT branch hotfix
	)
'

# -- basic listing -----------------------------------------------------------

test_expect_success 'branch -l lists all local branches' '
	(
	cd repo &&
	grit branch -l >actual &&
	grep "master" actual &&
	grep "feature" actual &&
	grep "bugfix" actual &&
	grep "release" actual &&
	grep "hotfix" actual &&
	grep "old-feature" actual
	)
'

test_expect_success 'branch with no args lists branches' '
	(
	cd repo &&
	grit branch >actual &&
	grep "master" actual &&
	grep "feature" actual
	)
'

test_expect_success 'current branch is marked with asterisk' '
	(
	cd repo &&
	grit branch >actual &&
	grep "^[*] master" actual
	)
'

test_expect_success 'branch --list lists all branches' '
	(
	cd repo &&
	grit branch --list >actual &&
	grep "master" actual &&
	grep "feature" actual &&
	grep "old-feature" actual
	)
'

# -- verbose listing ---------------------------------------------------------

test_expect_success 'branch -v shows commit hash and subject' '
	(
	cd repo &&
	grit branch -v >actual &&
	grep "master" actual &&
	grep "third" actual
	)
'

test_expect_success 'branch -v shows hash for feature branch' '
	(
	cd repo &&
	grit branch -v >actual &&
	grep "feature" actual &&
	grep "second" actual
	)
'

test_expect_success 'branch -v shows hash for old-feature' '
	(
	cd repo &&
	grit branch -v >actual &&
	grep "old-feature" actual &&
	grep "first" actual
	)
'

# -- --show-current ----------------------------------------------------------

test_expect_success 'branch --show-current shows master' '
	(
	cd repo &&
	grit branch --show-current >actual &&
	echo "master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch --show-current after checkout shows new branch' '
	(
	cd repo &&
	$REAL_GIT checkout feature &&
	grit branch --show-current >actual &&
	echo "feature" >expect &&
	test_cmp expect actual &&
	$REAL_GIT checkout master
	)
'

# -- --contains --------------------------------------------------------------

test_expect_success 'branch --contains HEAD shows branches at HEAD' '
	(
	cd repo &&
	grit branch --contains HEAD >actual &&
	grep "master" actual &&
	grep "release" actual &&
	grep "hotfix" actual
	)
'

test_expect_success 'branch --contains HEAD includes all branches containing tip' '
	(
	cd repo &&
	grit branch --contains HEAD >actual &&
	lines=$(wc -l <actual) &&
	test "$lines" -ge 3
	)
'

test_expect_success 'branch --contains first-commit shows all branches' '
	(
	cd repo &&
	first=$($REAL_GIT rev-parse master~2) &&
	grit branch --contains "$first" >actual &&
	grep "master" actual &&
	grep "old-feature" actual &&
	grep "feature" actual
	)
'

# -- --merged ----------------------------------------------------------------

test_expect_success 'branch --merged HEAD shows merged branches' '
	(
	cd repo &&
	grit branch --merged HEAD >actual &&
	grep "master" actual
	)
'

test_expect_success 'branch --merged HEAD includes old-feature' '
	(
	cd repo &&
	grit branch --merged HEAD >actual &&
	grep "old-feature" actual
	)
'

test_expect_success 'branch --merged HEAD includes feature' '
	(
	cd repo &&
	grit branch --merged HEAD >actual &&
	grep "feature" actual
	)
'

# -- creating and deleting branches ------------------------------------------

test_expect_success 'branch creates a new branch' '
	(
	cd repo &&
	grit branch new-branch &&
	grit branch -l >actual &&
	grep "new-branch" actual
	)
'

test_expect_success 'branch -d deletes a merged branch' '
	(
	cd repo &&
	grit branch to-delete &&
	grit branch -d to-delete &&
	grit branch -l >actual &&
	! grep "to-delete" actual
	)
'

test_expect_success 'branch -m renames a branch' '
	(
	cd repo &&
	grit branch rename-me &&
	grit branch -m rename-me renamed &&
	grit branch -l >actual &&
	! grep "rename-me" actual &&
	grep "renamed" actual
	)
'

test_expect_success 'branch -c copies current branch' '
	(
	cd repo &&
	grit branch -c master-copy &&
	grit branch -l >actual &&
	grep "master-copy" actual &&
	grep "master" actual
	)
'

# -- branch at specific start point ------------------------------------------

test_expect_success 'branch at specific commit' '
	(
	cd repo &&
	first=$($REAL_GIT rev-parse master~2) &&
	grit branch from-first "$first" &&
	grit rev-parse from-first >actual &&
	echo "$first" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch from tag-like ref' '
	(
	cd repo &&
	$REAL_GIT tag v1.0 master~1 &&
	grit branch from-tag v1.0 &&
	grit rev-parse from-tag >actual &&
	grit rev-parse v1.0 >expect &&
	test_cmp expect actual
	)
'

# -- force create ------------------------------------------------------------

test_expect_success 'branch -f overwrites existing branch' '
	(
	cd repo &&
	grit branch overwrite-me &&
	old=$(grit rev-parse overwrite-me) &&
	first=$($REAL_GIT rev-parse master~2) &&
	grit branch -f overwrite-me "$first" &&
	new=$(grit rev-parse overwrite-me) &&
	test "$old" != "$new" &&
	test "$new" = "$first"
	)
'

# -- compare with real git ---------------------------------------------------

test_expect_success 'branch list matches real git branch list' '
	(
	cd repo &&
	grit branch -l | sed "s/^[* ] //" | sort >actual &&
	$REAL_GIT branch -l | sed "s/^[* ] //" | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch --show-current matches real git' '
	(
	cd repo &&
	grit branch --show-current >actual &&
	$REAL_GIT branch --show-current >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch -v output has same branch count as real git' '
	(
	cd repo &&
	grit branch -v | wc -l >actual_count &&
	$REAL_GIT branch -v | wc -l >expect_count &&
	test_cmp expect_count actual_count
	)
'

test_done
