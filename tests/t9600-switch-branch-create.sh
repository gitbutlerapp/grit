#!/bin/sh
# Tests for grit switch: basic branch switching, -c (create),
# --detach, --discard-changes, error handling, and cross-checks.
# Note: grit switch forwards to system git, so we verify the forwarding works.

test_description='grit switch branch creation and switching'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with initial commit' '
	(
	$REAL_GIT init --initial-branch=master repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "base" >base.txt &&
	$REAL_GIT add base.txt &&
	test_tick &&
	$REAL_GIT commit -m "initial"
	)
'

###########################################################################
# Section 2: Basic switching
###########################################################################

test_expect_success 'switch to existing branch' '
	(
	cd repo &&
	$REAL_GIT branch feature1 &&
	grit switch feature1 &&
	test "$(grit branch --show-current)" = "feature1"
	)
'

test_expect_success 'switch back to master' '
	(
	cd repo &&
	grit switch master &&
	test "$(grit branch --show-current)" = "master"
	)
'

test_expect_success 'switch preserves working tree' '
	(
	cd repo &&
	grit switch feature1 &&
	test -f base.txt &&
	grep "base" base.txt
	)
'

test_expect_success 'switch back to master again' '
	(
	cd repo &&
	grit switch master &&
	test "$(grit branch --show-current)" = "master"
	)
'

###########################################################################
# Section 3: Create branch with -c
###########################################################################

test_expect_success 'switch -c creates new branch and switches to it' '
	(
	cd repo &&
	grit switch -c new-branch &&
	test "$(grit branch --show-current)" = "new-branch"
	)
'

test_expect_success 'switch -c new branch exists in branch list' '
	(
	cd repo &&
	grit branch -l >actual &&
	grep "new-branch" actual
	)
'

test_expect_success 'commit on new branch' '
	(
	cd repo &&
	echo "on new branch" >new-file.txt &&
	$REAL_GIT add new-file.txt &&
	test_tick &&
	$REAL_GIT commit -m "on new-branch"
	)
'

test_expect_success 'switch -c from specific commit' '
	(
	cd repo &&
	grit switch master &&
	echo "second" >second.txt &&
	$REAL_GIT add second.txt &&
	test_tick &&
	$REAL_GIT commit -m "second" &&
	grit switch -c from-first HEAD~1 &&
	test "$(grit branch --show-current)" = "from-first" &&
	! test -f second.txt
	)
'

test_expect_success 'switch -c fails if branch already exists' '
	(
	cd repo &&
	grit switch master &&
	test_must_fail grit switch -c new-branch
	)
'

test_expect_success 'switch -c with another name succeeds' '
	(
	cd repo &&
	grit switch -c yet-another &&
	test "$(grit branch --show-current)" = "yet-another"
	)
'

test_expect_success 'switch back to master for next section' '
	(
	cd repo &&
	grit switch master
	)
'

###########################################################################
# Section 4: Detached HEAD (--detach)
###########################################################################

test_expect_success 'switch --detach goes to detached HEAD' '
	(
	cd repo &&
	head_oid=$(grit rev-parse HEAD) &&
	grit switch --detach HEAD &&
	current=$(grit rev-parse HEAD) &&
	test "$head_oid" = "$current"
	)
'

test_expect_success 'switch --detach to specific commit' '
	(
	cd repo &&
	parent_oid=$(grit rev-parse HEAD~1) &&
	grit switch --detach HEAD~1 &&
	current=$(grit rev-parse HEAD) &&
	test "$parent_oid" = "$current"
	)
'

test_expect_success 'switch back to master from detached' '
	(
	cd repo &&
	grit switch master &&
	test "$(grit branch --show-current)" = "master"
	)
'

test_expect_success 'switch --detach to tag' '
	(
	cd repo &&
	$REAL_GIT tag v1.0 &&
	grit switch --detach v1.0 &&
	tag_oid=$(grit rev-parse v1.0) &&
	current=$(grit rev-parse HEAD) &&
	test "$tag_oid" = "$current"
	)
'

test_expect_success 'switch to master from tag detach' '
	(
	cd repo &&
	grit switch master
	)
'

###########################################################################
# Section 5: --discard-changes
###########################################################################

test_expect_success 'setup: create diverged branches for conflict' '
	(
	cd repo &&
	grit switch -c conflict-branch &&
	echo "conflict content" >base.txt &&
	$REAL_GIT add base.txt &&
	test_tick &&
	$REAL_GIT commit -m "conflict on branch" &&
	grit switch master
	)
'

test_expect_success 'switch fails with conflicting uncommitted changes' '
	(
	cd repo &&
	echo "dirty local" >base.txt &&
	test_must_fail grit switch conflict-branch
	)
'

test_expect_success 'switch --discard-changes ignores dirty working tree' '
	(
	cd repo &&
	grit switch --discard-changes conflict-branch &&
	test "$(grit branch --show-current)" = "conflict-branch"
	)
'

test_expect_success 'working tree matches branch content after --discard-changes' '
	(
	cd repo &&
	grep "conflict content" base.txt
	)
'

test_expect_success 'switch back to master (clean)' '
	(
	cd repo &&
	grit switch master
	)
'

###########################################################################
# Section 6: Switching with commits on different branches
###########################################################################

test_expect_success 'file exists on one branch but not another' '
	(
	cd repo &&
	grit switch -c has-file &&
	echo "branch-only" >branch-only.txt &&
	$REAL_GIT add branch-only.txt &&
	test_tick &&
	$REAL_GIT commit -m "branch only file" &&
	grit switch master &&
	! test -f branch-only.txt
	)
'

test_expect_success 'switching back restores branch-specific file' '
	(
	cd repo &&
	grit switch has-file &&
	test -f branch-only.txt &&
	grep "branch-only" branch-only.txt
	)
'

test_expect_success 'switch master for next test' '
	(
	cd repo &&
	grit switch master
	)
'

test_expect_success 'multiple rapid switches' '
	(
	cd repo &&
	grit switch feature1 &&
	grit switch has-file &&
	grit switch master &&
	test "$(grit branch --show-current)" = "master"
	)
'

###########################################################################
# Section 7: Error cases
###########################################################################

test_expect_success 'switch to nonexistent branch fails' '
	(
	cd repo &&
	test_must_fail grit switch no-such-branch
	)
'

test_expect_success 'switch with no arguments fails' '
	(
	cd repo &&
	test_must_fail grit switch
	)
'

test_expect_success 'switch -c with invalid branch name fails' '
	(
	cd repo &&
	test_must_fail grit switch -c "bad..name"
	)
'

test_expect_success 'switch to current branch is ok' '
	(
	cd repo &&
	grit switch master &&
	test "$(grit branch --show-current)" = "master"
	)
'

###########################################################################
# Section 8: --orphan
###########################################################################

test_expect_success 'switch --orphan creates orphan branch' '
	(
	cd repo &&
	grit switch --orphan orphan-branch &&
	test "$(grit branch --show-current)" = "orphan-branch"
	)
'

test_expect_success 'orphan branch has empty index' '
	(
	cd repo &&
	grit ls-files >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'can commit on orphan branch' '
	(
	cd repo &&
	echo "orphan content" >orphan.txt &&
	$REAL_GIT add orphan.txt &&
	test_tick &&
	$REAL_GIT commit -m "first orphan commit" &&
	grit log --oneline >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'switch back to master from orphan' '
	(
	cd repo &&
	grit switch master &&
	test "$(grit branch --show-current)" = "master"
	)
'

###########################################################################
# Section 9: Cross-check with real git
###########################################################################

test_expect_success 'setup cross-check repos' '
	(
	$REAL_GIT init --initial-branch=master git-cmp &&
	cd git-cmp &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "a" >a.txt &&
	$REAL_GIT add a.txt &&
	test_tick &&
	$REAL_GIT commit -m "init" &&
	cd .. &&
	grit init --initial-branch=master grit-cmp &&
	cd grit-cmp &&
	echo "a" >a.txt &&
	grit add a.txt &&
	test_tick &&
	grit commit -m "init"
	)
'

test_expect_success 'switch -c: both create same branch' '
	$REAL_GIT -C git-cmp switch -c test-br &&
	grit -C grit-cmp switch -c test-br &&
	git_br=$($REAL_GIT -C git-cmp branch --show-current) &&
	grit_br=$(grit -C grit-cmp branch --show-current) &&
	test "$git_br" = "$grit_br"
'

test_expect_success 'switch back: both return to master' '
	$REAL_GIT -C git-cmp switch master &&
	grit -C grit-cmp switch master &&
	git_br=$($REAL_GIT -C git-cmp branch --show-current) &&
	grit_br=$(grit -C grit-cmp branch --show-current) &&
	test "$git_br" = "$grit_br"
'

test_expect_success 'switch -c in cross-check creates and lists' '
	$REAL_GIT -C git-cmp switch -c cross-br &&
	grit -C grit-cmp switch -c cross-br &&
	$REAL_GIT -C git-cmp switch master &&
	grit -C grit-cmp switch master &&
	$REAL_GIT -C git-cmp branch >expect &&
	grit -C grit-cmp branch >actual &&
	test_cmp expect actual
'

test_expect_success 'switch --detach works in cross-check' '
	grit -C grit-cmp switch --detach HEAD &&
	grit_head=$(grit -C grit-cmp rev-parse HEAD) &&
	test -n "$grit_head" &&
	grit -C grit-cmp switch master
'

test_expect_success 'branch list matches after switch operations' '
	$REAL_GIT -C git-cmp branch >expect &&
	grit -C grit-cmp branch >actual &&
	test_cmp expect actual
'

test_done
