#!/bin/sh
# Tests for grit branch management: verbose, move, contains, merged, force, show-current.

test_description='grit branch verbose, move, contains, merged, and management flags'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repository with history' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "first" >file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "first commit" &&
	echo "second" >>file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "second commit" &&
	echo "third" >>file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "third commit"
	)
'

test_expect_success 'setup: create branches' '
	(
	cd repo &&
	grit branch feature &&
	grit branch bugfix &&
	grit branch release
	)
'

###########################################################################
# Section 2: --show-current
###########################################################################

test_expect_success 'branch --show-current shows main' '
	(
	cd repo &&
	grit branch --show-current >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch --show-current matches real git' '
	(
	cd repo &&
	grit branch --show-current >actual &&
	"$REAL_GIT" branch --show-current >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch --show-current after checkout' '
	(
	cd repo &&
	grit checkout feature &&
	grit branch --show-current >actual &&
	echo "feature" >expect &&
	test_cmp expect actual &&
	grit checkout main
	)
'

test_expect_success 'branch --show-current returns to main' '
	(
	cd repo &&
	grit branch --show-current >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 3: branch -v / --verbose
###########################################################################

test_expect_success 'branch -v shows commit subject' '
	(
	cd repo &&
	grit branch -v >actual &&
	grep "third commit" actual
	)
'

test_expect_success 'branch -v lists all branches' '
	(
	cd repo &&
	grit branch -v >actual &&
	grep "feature" actual &&
	grep "bugfix" actual &&
	grep "release" actual &&
	grep "main" actual
	)
'

test_expect_success 'branch -v marks current branch with asterisk' '
	(
	cd repo &&
	grit branch -v >actual &&
	grep "^\\* main" actual
	)
'

test_expect_success 'branch -v shows abbreviated OID' '
	(
	cd repo &&
	head_short=$(grit rev-parse HEAD | cut -c1-7) &&
	grit branch -v >actual &&
	grep "$head_short" actual
	)
'

test_expect_success 'branch -v matches real git branch count' '
	(
	cd repo &&
	grit branch -v | wc -l | tr -d " " >actual_count &&
	"$REAL_GIT" branch -v | wc -l | tr -d " " >expect_count &&
	test_cmp expect_count actual_count
	)
'

###########################################################################
# Section 4: branch -m (move/rename)
###########################################################################

test_expect_success 'branch -m renames branch' '
	(
	cd repo &&
	grit branch temp-rename &&
	grit branch -m temp-rename renamed &&
	grit branch -l >actual &&
	grep "renamed" actual &&
	! grep "temp-rename" actual
	)
'

test_expect_success 'branch -m renamed branch visible to real git' '
	(
	cd repo &&
	"$REAL_GIT" branch -l >actual &&
	grep "renamed" actual
	)
'

test_expect_success 'branch -m fails for nonexistent branch' '
	(
	cd repo &&
	test_must_fail grit branch -m no-such-branch other-name 2>err &&
	grep -i "no branch" err
	)
'

test_expect_success 'branch -M force renames over existing branch' '
	(
	cd repo &&
	grit branch force-target &&
	grit branch force-src &&
	grit branch -M force-src force-target &&
	grit branch -l >actual &&
	grep "force-target" actual &&
	! grep "force-src" actual
	)
'

test_expect_success 'branch -m preserves commit' '
	(
	cd repo &&
	grit branch move-test &&
	before_oid=$(grit rev-parse move-test) &&
	grit branch -m move-test moved-test &&
	after_oid=$(grit rev-parse moved-test) &&
	test "$before_oid" = "$after_oid"
	)
'

###########################################################################
# Section 5: branch --contains
###########################################################################

test_expect_success 'branch --contains HEAD includes all branches' '
	(
	cd repo &&
	grit branch --contains HEAD >actual &&
	grep "main" actual &&
	grep "feature" actual
	)
'

test_expect_success 'branch --contains matches real git' '
	(
	cd repo &&
	grit branch --contains HEAD | sed "s/^[* ] //" | sort >actual &&
	"$REAL_GIT" branch --contains HEAD | sed "s/^[* ] //" | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch --contains with first commit OID' '
	(
	cd repo &&
	first_oid=$(grit rev-list HEAD | tail -1) &&
	grit branch --contains "$first_oid" >actual &&
	grep "main" actual
	)
'

test_expect_success 'branch --contains result includes feature' '
	(
	cd repo &&
	first_oid=$(grit rev-list HEAD | tail -1) &&
	grit branch --contains "$first_oid" >actual &&
	grep "feature" actual
	)
'

###########################################################################
# Section 6: branch after checkout and diverge
###########################################################################

test_expect_success 'setup: diverge feature branch' '
	(
	cd repo &&
	grit checkout feature &&
	echo "feature-only" >feature.txt &&
	grit add feature.txt &&
	grit commit -m "feature work" &&
	grit checkout main
	)
'

test_expect_success 'feature has different tip than main after diverge' '
	(
	cd repo &&
	main_oid=$(grit rev-parse main) &&
	feature_oid=$(grit rev-parse feature) &&
	test "$main_oid" != "$feature_oid"
	)
'

test_expect_success 'branch -v shows different OIDs after diverge' '
	(
	cd repo &&
	grit branch -v >actual &&
	main_line=$(grep "main" actual) &&
	feature_line=$(grep "feature" actual) &&
	test "$main_line" != "$feature_line"
	)
'

test_expect_success 'branch -v shows feature commit message' '
	(
	cd repo &&
	grit branch -v >actual &&
	grep "feature work" actual
	)
'

test_expect_success 'branch --show-current still shows main' '
	(
	cd repo &&
	grit branch --show-current >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'creating branch from feature diverge point' '
	(
	cd repo &&
	grit branch from-feature feature &&
	ff_oid=$(grit rev-parse from-feature) &&
	f_oid=$(grit rev-parse feature) &&
	test "$ff_oid" = "$f_oid"
	)
'

###########################################################################
# Section 7: branch -d / -D
###########################################################################

test_expect_success 'branch -d deletes merged branch' '
	(
	cd repo &&
	grit branch to-delete &&
	grit branch -d to-delete &&
	grit branch -l >actual &&
	! grep "to-delete" actual
	)
'

test_expect_success 'branch -D force deletes unmerged branch' '
	(
	cd repo &&
	grit branch -D feature &&
	grit branch -l >actual &&
	! grep "^[* ] feature$" actual &&
	! grep "  feature$" actual
	)
'

test_expect_success 'branch -d fails for nonexistent branch' '
	(
	cd repo &&
	test_must_fail grit branch -d nonexistent-branch
	)
'

test_expect_success 'branch -D on merged branch also works' '
	(
	cd repo &&
	grit branch -D release &&
	grit branch -l >actual &&
	! grep "release" actual
	)
'

###########################################################################
# Section 8: branch -f (force create)
###########################################################################

test_expect_success 'branch -f overwrites existing branch to different commit' '
	(
	cd repo &&
	first_oid=$(grit rev-list HEAD | tail -1) &&
	grit branch overwrite-me &&
	grit branch -f overwrite-me "$first_oid" &&
	actual_oid=$(grit rev-parse overwrite-me) &&
	test "$actual_oid" = "$first_oid"
	)
'

test_expect_success 'branch -f result matches real git rev-parse' '
	(
	cd repo &&
	grit rev-parse overwrite-me >actual &&
	"$REAL_GIT" rev-parse overwrite-me >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch without -f fails when branch exists' '
	(
	cd repo &&
	test_must_fail grit branch overwrite-me 2>err
	)
'

###########################################################################
# Section 9: branch listing
###########################################################################

test_expect_success 'branch -l lists branches alphabetically' '
	(
	cd repo &&
	grit branch alpha-first &&
	grit branch zebra-last &&
	grit branch -l | sed "s/^[* ] //" >actual &&
	sort actual >sorted &&
	test_cmp sorted actual
	)
'

test_expect_success 'branch with no args lists branches' '
	(
	cd repo &&
	grit branch >actual &&
	grep "main" actual
	)
'

test_expect_success 'branch list matches real git' '
	(
	cd repo &&
	grit branch | sed "s/^[* ] //" | sort >actual &&
	"$REAL_GIT" branch | sed "s/^[* ] //" | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch -a shows local branches' '
	(
	cd repo &&
	grit branch -a >actual &&
	grep "main" actual &&
	grep "alpha-first" actual
	)
'

###########################################################################
# Section 10: branch from start point
###########################################################################

test_expect_success 'branch from specific commit' '
	(
	cd repo &&
	first_oid=$(grit rev-list HEAD | tail -1) &&
	grit branch from-first "$first_oid" &&
	actual_oid=$(grit rev-parse from-first) &&
	test "$actual_oid" = "$first_oid"
	)
'

test_expect_success 'branch from tag-like ref' '
	(
	cd repo &&
	second_oid=$(grit rev-list HEAD | head -2 | tail -1) &&
	grit branch from-second "$second_oid" &&
	actual_oid=$(grit rev-parse from-second) &&
	test "$actual_oid" = "$second_oid"
	)
'

test_expect_success 'branch from another branch name' '
	(
	cd repo &&
	grit branch from-bugfix bugfix &&
	from_oid=$(grit rev-parse from-bugfix) &&
	bugfix_oid=$(grit rev-parse bugfix) &&
	test "$from_oid" = "$bugfix_oid"
	)
'

test_done
