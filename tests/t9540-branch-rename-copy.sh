#!/bin/sh
# Tests for grit branch: rename (-m/-M), delete (-d/-D),
# create, list, --contains, --merged, --no-merged, --show-current, -v.

test_description='grit branch rename, delete, and management'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with commits' '
	(
	grit init --initial-branch=master repo &&
	cd repo &&
	grit config set user.name "Test" &&
	grit config set user.email "test@test.com" &&
	echo "first" >file.txt &&
	grit add file.txt &&
	grit commit -m "first commit" &&
	echo "second" >file2.txt &&
	grit add file2.txt &&
	grit commit -m "second commit" &&
	echo "third" >file3.txt &&
	grit add file3.txt &&
	grit commit -m "third commit"
	)
'

test_expect_success 'save commit SHAs for later tests' '
	(
	cd repo &&
	grit rev-parse HEAD >../sha_head &&
	grit rev-parse HEAD~1 >../sha_parent &&
	grit rev-parse HEAD~2 >../sha_first
	)
'

###########################################################################
# Section 2: Branch creation
###########################################################################

test_expect_success 'create a branch' '
	(
	cd repo &&
	grit branch feature &&
	grit branch -l >actual &&
	grep "feature" actual
	)
'

test_expect_success 'create branch at specific commit via SHA' '
	(
	cd repo &&
	grit branch older $(cat ../sha_parent) &&
	grit rev-parse older >actual &&
	test_cmp ../sha_parent actual
	)
'

test_expect_success 'create branch fails if name exists' '
	(
	cd repo &&
	test_must_fail grit branch feature
	)
'

test_expect_success 'create branch with --force overwrites' '
	(
	cd repo &&
	grit branch --force feature $(cat ../sha_parent) &&
	grit rev-parse feature >actual &&
	test_cmp ../sha_parent actual
	)
'

test_expect_success 'reset feature back to HEAD' '
	(
	cd repo &&
	grit branch -f feature HEAD
	)
'

###########################################################################
# Section 3: Branch listing
###########################################################################

test_expect_success 'branch -l lists all local branches' '
	(
	cd repo &&
	grit branch -l >actual &&
	grep "master" actual &&
	grep "feature" actual &&
	grep "older" actual
	)
'

test_expect_success 'branch with no args lists branches' '
	(
	cd repo &&
	grit branch >actual &&
	grep "master" actual
	)
'

test_expect_success 'current branch is marked with asterisk' '
	(
	cd repo &&
	grit branch >actual &&
	grep "^\*" actual | grep "master"
	)
'

test_expect_success 'branch -v shows commit subject' '
	(
	cd repo &&
	grit branch -v >actual &&
	grep "third commit" actual
	)
'

test_expect_success 'branch -v shows abbreviated SHA' '
	(
	cd repo &&
	grit branch -v >actual &&
	head_short=$(cat ../sha_head | cut -c1-7) &&
	grep "$head_short" actual
	)
'

###########################################################################
# Section 4: --show-current
###########################################################################

test_expect_success 'branch --show-current shows current branch' '
	(
	cd repo &&
	grit branch --show-current >actual &&
	echo "master" >expect &&
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
	grit checkout master
	)
'

###########################################################################
# Section 5: Rename (-m)
###########################################################################

test_expect_success 'rename branch with -m' '
	(
	cd repo &&
	grit branch rename-src &&
	grit branch -m rename-src rename-dst &&
	grit branch -l >actual &&
	grep "rename-dst" actual &&
	! grep "rename-src" actual
	)
'

test_expect_success 'rename fails if target exists' '
	(
	cd repo &&
	grit branch rename-target &&
	test_must_fail grit branch -m rename-dst rename-target
	)
'

test_expect_success 'force rename with -M overwrites target' '
	(
	cd repo &&
	grit rev-parse rename-dst >before &&
	grit branch -M rename-dst rename-target &&
	grit rev-parse rename-target >after &&
	test_cmp before after &&
	grit branch -l >actual &&
	! grep "rename-dst" actual
	)
'

test_expect_success 'rename branch preserves commit pointer' '
	(
	cd repo &&
	grit branch pre-rename &&
	grit rev-parse pre-rename >before &&
	grit branch -m pre-rename post-rename &&
	grit rev-parse post-rename >after &&
	test_cmp before after
	)
'

test_expect_success 'rename to different name succeeds' '
	(
	cd repo &&
	grit branch rename-a &&
	grit branch -m rename-a rename-b &&
	grit branch -l >actual &&
	grep "rename-b" actual &&
	! grep "rename-a" actual &&
	grit branch -d rename-b
	)
'

###########################################################################
# Section 6: Delete (-d / -D)
###########################################################################

test_expect_success 'delete branch with -d' '
	(
	cd repo &&
	grit branch delete-me &&
	grit branch -d delete-me &&
	grit branch -l >actual &&
	! grep "delete-me" actual
	)
'

test_expect_success 'delete fails for non-existent branch' '
	(
	cd repo &&
	test_must_fail grit branch -d nonexistent
	)
'

test_expect_success 'force delete with -D' '
	(
	cd repo &&
	grit branch force-del &&
	grit branch -D force-del &&
	grit branch -l >actual &&
	! grep "force-del" actual
	)
'

test_expect_success 'cannot delete current branch' '
	(
	cd repo &&
	test_must_fail grit branch -d master
	)
'

test_expect_success '-d shows deleted branch info' '
	(
	cd repo &&
	grit branch show-del &&
	grit branch -d show-del >actual 2>&1 &&
	grep "Deleted" actual
	)
'

test_expect_success 'delete then recreate branch' '
	(
	cd repo &&
	grit branch recreate &&
	grit branch -d recreate &&
	grit branch recreate &&
	grit branch -l >actual &&
	grep "recreate" actual &&
	grit branch -d recreate
	)
'

###########################################################################
# Section 7: --contains
###########################################################################

test_expect_success 'branch --contains HEAD includes master' '
	(
	cd repo &&
	grit branch --contains HEAD >actual &&
	grep "master" actual
	)
'

test_expect_success 'branch --contains first commit includes all branches' '
	(
	cd repo &&
	grit branch --contains $(cat ../sha_first) >actual &&
	grep "master" actual &&
	grep "feature" actual
	)
'

test_expect_success 'branch --contains parent commit' '
	(
	cd repo &&
	grit branch --contains $(cat ../sha_parent) >actual &&
	grep "master" actual
	)
'

###########################################################################
# Section 8: --merged / --no-merged
###########################################################################

test_expect_success 'branch --merged HEAD lists merged branches' '
	(
	cd repo &&
	grit branch --merged HEAD >actual &&
	grep "master" actual
	)
'

test_expect_success 'setup divergent branch' '
	(
	cd repo &&
	grit checkout -b diverge $(cat ../sha_parent) &&
	echo "diverge content" >diverge.txt &&
	grit add diverge.txt &&
	grit commit -m "diverge commit" &&
	grit checkout master
	)
'

test_expect_success 'branch --no-merged master lists diverge' '
	(
	cd repo &&
	grit branch --no-merged master >actual &&
	grep "diverge" actual
	)
'

test_expect_success 'branch --merged includes feature (same commit)' '
	(
	cd repo &&
	grit branch --merged HEAD >actual &&
	grep "feature" actual
	)
'

###########################################################################
# Section 9: Comparison with real git
###########################################################################

test_expect_success 'grit and git agree on --show-current' '
	(
	cd repo &&
	grit branch --show-current >grit_cur &&
	$REAL_GIT branch --show-current >git_cur &&
	test_cmp git_cur grit_cur
	)
'

test_expect_success 'grit and git branch list match' '
	(
	cd repo &&
	grit branch -l >grit_out &&
	$REAL_GIT branch -l >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 10: Edge cases
###########################################################################

test_expect_success 'branch -q suppresses output on creation' '
	(
	cd repo &&
	grit branch -q quiet-branch >actual 2>&1 &&
	test_must_be_empty actual &&
	grit branch -d quiet-branch
	)
'

test_expect_success 'branch with start point tag' '
	(
	cd repo &&
	grit tag anchor-tag $(cat ../sha_parent) &&
	grit branch from-tag anchor-tag &&
	grit rev-parse from-tag >actual &&
	test_cmp ../sha_parent actual
	)
'

test_expect_success 'creating branch does not switch to it' '
	(
	cd repo &&
	grit branch nosw &&
	grit branch --show-current >actual &&
	echo "master" >expect &&
	test_cmp expect actual &&
	grit branch -d nosw
	)
'

test_expect_success 'branch at HEAD is default' '
	(
	cd repo &&
	grit branch at-head &&
	grit rev-parse at-head >actual &&
	grit rev-parse HEAD >expect &&
	test_cmp expect actual &&
	grit branch -d at-head
	)
'

test_done
