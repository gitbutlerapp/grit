#!/bin/sh
# Tests for branch -d, -D (force delete), -m/-M (rename), -c (copy),
# --force creation, and error handling on delete of current/nonexistent.

test_description='branch delete, force-delete, rename, copy'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- setup ------------------------------------------------------------------

test_expect_success 'setup: create repo with commits on multiple branches' '
	(
	$REAL_GIT init --initial-branch=master repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "base" >file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "initial" &&
	$REAL_GIT checkout -b merged-branch &&
	echo "merged" >merged.txt &&
	$REAL_GIT add merged.txt &&
	test_tick &&
	$REAL_GIT commit -m "on merged-branch" &&
	$REAL_GIT checkout master &&
	$REAL_GIT merge --no-edit merged-branch &&
	$REAL_GIT checkout -b unmerged-branch &&
	echo "unmerged" >unmerged.txt &&
	$REAL_GIT add unmerged.txt &&
	test_tick &&
	$REAL_GIT commit -m "on unmerged-branch" &&
	$REAL_GIT checkout master &&
	$REAL_GIT checkout -b extra1 &&
	$REAL_GIT checkout master &&
	$REAL_GIT checkout -b extra2 &&
	$REAL_GIT checkout master
	)
'

# -- delete merged branch ---------------------------------------------------

test_expect_success 'branch -d deletes merged branch' '
	(
	cd repo &&
	grit branch -d merged-branch &&
	grit branch -l >actual &&
	! grep -w "merged-branch" actual
	)
'

test_expect_success 'branch -D force-deletes unmerged branch' '
	(
	cd repo &&
	grit branch -D unmerged-branch &&
	grit branch -l >actual &&
	! grep "unmerged-branch" actual
	)
'

# -- force delete -----------------------------------------------------------

test_expect_success 'branch -D force deletes a branch' '
	(
	cd repo &&
	$REAL_GIT checkout -b force-del-test &&
	echo "fd" >fd.txt &&
	$REAL_GIT add fd.txt &&
	test_tick &&
	$REAL_GIT commit -m "force-del" &&
	$REAL_GIT checkout master &&
	grit branch -D force-del-test &&
	grit branch -l >actual &&
	! grep "force-del-test" actual
	)
'

test_expect_success 'branch -D on nonexistent fails' '
	(
	cd repo &&
	! grit branch -D ghost-branch 2>err &&
	test -s err
	)
'

test_expect_success 'branch -D deletes merged branch too' '
	(
	cd repo &&
	$REAL_GIT checkout -b to-force-del &&
	echo "x" >x.txt &&
	$REAL_GIT add x.txt &&
	test_tick &&
	$REAL_GIT commit -m "for force del" &&
	$REAL_GIT checkout master &&
	$REAL_GIT merge --no-edit to-force-del &&
	grit branch -D to-force-del &&
	grit branch -l >actual &&
	! grep "to-force-del" actual
	)
'

# -- cannot delete current branch -------------------------------------------

test_expect_success 'branch -d cannot delete current branch' '
	(
	cd repo &&
	! grit branch -d master 2>err &&
	grep -i "cannot delete.*checked out\|Cannot delete" err
	)
'

test_expect_success 'branch -D cannot delete current branch' '
	(
	cd repo &&
	! grit branch -D master 2>err &&
	grep -i "cannot delete.*checked out\|Cannot delete" err
	)
'

# -- delete nonexistent branch ----------------------------------------------

test_expect_success 'branch -d nonexistent branch fails' '
	(
	cd repo &&
	! grit branch -d no-such-branch 2>err &&
	grep -i "not found\|does not exist\|error" err
	)
'

# -- rename -----------------------------------------------------------------

test_expect_success 'branch -m renames a branch' '
	(
	cd repo &&
	$REAL_GIT checkout -b rename-me &&
	$REAL_GIT checkout master &&
	grit branch -m rename-me renamed &&
	grit branch -l >actual &&
	grep "renamed" actual &&
	! grep "rename-me" actual
	)
'

test_expect_success 'branch -m renames current branch' '
	(
	cd repo &&
	$REAL_GIT checkout renamed &&
	grit branch -m renamed new-name &&
	grit branch --show-current >actual &&
	echo "new-name" >expect &&
	test_cmp expect actual &&
	$REAL_GIT checkout master
	)
'

test_expect_success 'branch -m fails if target exists' '
	(
	cd repo &&
	$REAL_GIT checkout -b src-br &&
	$REAL_GIT checkout master &&
	! grit branch -m src-br extra1 2>err
	)
'

test_expect_success 'branch -M force renames over existing' '
	(
	cd repo &&
	grit branch -M src-br extra1 &&
	grit branch -l >actual &&
	! grep "src-br" actual
	)
'

# -- copy -------------------------------------------------------------------

test_expect_success 'branch create at specific start point' '
	(
	cd repo &&
	grit branch from-head master &&
	grit rev-parse from-head >actual &&
	grit rev-parse master >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch -c copies current branch to new name' '
	(
	cd repo &&
	$REAL_GIT checkout extra2 &&
	grit branch -c copy-extra2 &&
	grit branch -l >actual &&
	grep "copy-extra2" actual &&
	$REAL_GIT checkout master
	)
'

# -- create with --force -----------------------------------------------------

test_expect_success 'branch --force overwrites existing branch' '
	(
	cd repo &&
	grit rev-parse master >old_head &&
	$REAL_GIT checkout -b force-target &&
	echo "new" >new.txt &&
	$REAL_GIT add new.txt &&
	test_tick &&
	$REAL_GIT commit -m "diverged" &&
	$REAL_GIT checkout master &&
	grit branch --force force-target master &&
	grit rev-parse force-target >actual &&
	grit rev-parse master >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch without --force fails for existing branch' '
	(
	cd repo &&
	! grit branch extra1 2>err
	)
'

# -- delete multiple branches -----------------------------------------------

test_expect_success 'setup: create branches for multi-delete' '
	(
	cd repo &&
	$REAL_GIT checkout -b del-a && $REAL_GIT checkout master &&
	$REAL_GIT checkout -b del-b && $REAL_GIT checkout master &&
	$REAL_GIT checkout -b del-c && $REAL_GIT checkout master
	)
'

test_expect_success 'branch -d deletes first multi-delete branch' '
	(
	cd repo &&
	grit branch -d del-a &&
	grit branch -l >actual &&
	! grep "del-a" actual
	)
'

test_expect_success 'branch -d deletes second multi-delete branch' '
	(
	cd repo &&
	grit branch -d del-b &&
	grit branch -l >actual &&
	! grep "del-b" actual
	)
'

test_expect_success 'branch -d deletes third multi-delete branch' '
	(
	cd repo &&
	grit branch -d del-c &&
	grit branch -l >actual &&
	! grep "del-c" actual
	)
'

# -- comparison with real git ------------------------------------------------

test_expect_success 'setup: comparison repos for branch delete' '
	(
	$REAL_GIT init --initial-branch=master git-cmp &&
	cd git-cmp &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "data" >f.txt &&
	$REAL_GIT add f.txt &&
	test_tick &&
	$REAL_GIT commit -m "init" &&
	$REAL_GIT checkout -b cmp-branch &&
	$REAL_GIT checkout master &&
	cd .. &&
	$REAL_GIT clone git-cmp grit-cmp &&
	cd grit-cmp && $REAL_GIT checkout -b cmp-branch origin/cmp-branch && $REAL_GIT checkout master && cd ..
	)
'

test_expect_success 'branch -d output matches: branch is gone' '
	$REAL_GIT -C git-cmp branch -d cmp-branch &&
	grit -C grit-cmp branch -d cmp-branch &&
	$REAL_GIT -C git-cmp branch -l >git-branches &&
	grit -C grit-cmp branch -l >grit-branches &&
	! grep "cmp-branch" git-branches &&
	! grep "cmp-branch" grit-branches
'

test_expect_success 'branch -l after delete: branch count matches' '
	$REAL_GIT -C git-cmp branch -l >expect &&
	grit -C grit-cmp branch -l >actual &&
	test "$(wc -l <expect)" = "$(wc -l <actual)"
'

# -- quiet flag --------------------------------------------------------------

test_expect_success 'branch -d -q suppresses output' '
	(
	cd repo &&
	$REAL_GIT checkout -b quiet-del && $REAL_GIT checkout master &&
	grit branch -d -q quiet-del >actual 2>&1 &&
	test_must_be_empty actual
	)
'

test_done
