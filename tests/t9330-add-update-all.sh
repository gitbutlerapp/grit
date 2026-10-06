#!/bin/sh
# Tests for add -u/--update, -A/--all, -n/--dry-run, -v/--verbose,
# -N/--intent-to-add, -f/--force, pathspec matching, and error cases.

test_description='add -u, -A, --dry-run, --intent-to-add, --force, pathspecs'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- setup ------------------------------------------------------------------

test_expect_success 'setup: init repo with tracked files' '
	(
	grit init repo &&
	cd repo &&
	echo "a" >tracked1.txt &&
	echo "b" >tracked2.txt &&
	mkdir subdir &&
	echo "c" >subdir/tracked3.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "initial"
	)
'

# -- basic add ---------------------------------------------------------------

test_expect_success 'add stages a new file' '
	(
	cd repo &&
	echo "new" >new.txt &&
	grit add new.txt &&
	grit status --porcelain >actual &&
	grep "^A  new.txt" actual
	)
'

test_expect_success 'add stages modified file' '
	(
	cd repo &&
	echo "modified" >>tracked1.txt &&
	grit add tracked1.txt &&
	grit status --porcelain >actual &&
	grep "^M  tracked1.txt" actual
	)
'

test_expect_success 'add with dot stages everything' '
	(
	cd repo &&
	echo "another" >another.txt &&
	grit add . &&
	grit status --porcelain >actual &&
	grep "^A  another.txt" actual
	)
'

# -- add -u / --update -------------------------------------------------------

test_expect_success 'setup: commit current state for -u tests' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "before update tests"
	)
'

test_expect_success 'add -u stages modified tracked file' '
	(
	cd repo &&
	echo "updated" >>tracked2.txt &&
	grit add -u &&
	grit status --porcelain >actual &&
	grep "^M  tracked2.txt" actual
	)
'

test_expect_success 'add -u does not add untracked files' '
	(
	cd repo &&
	echo "untracked" >untracked-u.txt &&
	grit add -u &&
	grit status --porcelain >actual &&
	grep "^?? untracked-u.txt" actual
	)
'

test_expect_success 'add -u stages deleted tracked files' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "before delete" &&
	rm tracked2.txt &&
	grit add -u &&
	grit status --porcelain >actual &&
	grep "^D  tracked2.txt" actual
	)
'

test_expect_success 'add --update is same as -u' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "after delete" &&
	echo "re-create" >tracked1.txt &&
	grit add tracked1.txt &&
	test_tick &&
	grit commit -m "recreated" &&
	echo "update again" >>tracked1.txt &&
	grit add --update &&
	grit status --porcelain >actual &&
	grep "^M  tracked1.txt" actual
	)
'

# -- add -A / --all ----------------------------------------------------------

test_expect_success 'add -A stages new, modified, and deleted files' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "base for -A" &&
	echo "brand new" >brandnew.txt &&
	echo "mod" >>tracked1.txt &&
	rm -f subdir/tracked3.txt &&
	grit add -A &&
	grit status --porcelain >actual &&
	grep "^A  brandnew.txt" actual &&
	grep "^M  tracked1.txt" actual &&
	grep "^D  subdir/tracked3.txt" actual
	)
'

test_expect_success 'add --all is same as -A' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "after -A" &&
	echo "new2" >new2.txt &&
	grit add --all &&
	grit status --porcelain >actual &&
	grep "new2.txt" actual
	)
'

# -- add -n / --dry-run ------------------------------------------------------

test_expect_success 'add --dry-run does not actually stage' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "clean state" &&
	echo "dry" >dry.txt &&
	grit add --dry-run dry.txt >output &&
	grit status --porcelain >actual &&
	grep "^?? dry.txt" actual
	)
'

test_expect_success 'add -n is same as --dry-run' '
	(
	cd repo &&
	grit add -n dry.txt >output &&
	grit status --porcelain >actual &&
	grep "^?? dry.txt" actual
	)
'

# -- add -v / --verbose ------------------------------------------------------

test_expect_success 'add -v produces output' '
	(
	cd repo &&
	grit add -v dry.txt >actual &&
	test -s actual
	)
'

test_expect_success 'add --verbose produces output' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "with dry" &&
	echo "verbose" >verbose.txt &&
	grit add --verbose verbose.txt >actual &&
	test -s actual
	)
'

# -- add -N / --intent-to-add -----------------------------------------------

test_expect_success 'add -N marks file as intent-to-add' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "before ita" &&
	echo "intent" >intent.txt &&
	grit add -N intent.txt &&
	grit status --porcelain >actual &&
	grep "intent.txt" actual
	)
'

test_expect_success 'add --intent-to-add is same as -N' '
	(
	cd repo &&
	echo "intent2" >intent2.txt &&
	grit add --intent-to-add intent2.txt &&
	grit status --porcelain >actual &&
	grep "intent2.txt" actual
	)
'

# -- add -f / --force (ignored files) ----------------------------------------

test_expect_success 'setup: create .gitignore' '
	(
	cd repo &&
	echo "*.log" >.gitignore &&
	grit add .gitignore &&
	test_tick &&
	grit commit -m "add gitignore"
	)
'

test_expect_success 'add refuses ignored file without -f' '
	(
	cd repo &&
	echo "log data" >debug.log &&
	test_must_fail grit add debug.log 2>err &&
	grep -i "ignored" err
	)
'

test_expect_success 'add -f stages ignored files' '
	(
	cd repo &&
	echo "forced" >forced.log &&
	grit add -f forced.log &&
	grit status --porcelain >actual &&
	grep "forced.log" actual
	)
'

test_expect_success 'add --force is same as -f' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "with log" &&
	echo "another log" >other.log &&
	grit add --force other.log &&
	grit status --porcelain >actual &&
	grep "other.log" actual
	)
'

# -- pathspec ----------------------------------------------------------------

test_expect_success 'add with specific pathspec' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "clean" &&
	echo "x" >pathspec-a.txt &&
	echo "y" >pathspec-b.txt &&
	grit add pathspec-a.txt &&
	grit status --porcelain >actual &&
	grep "pathspec-a.txt" actual &&
	grep "^?? pathspec-b.txt" actual
	)
'

test_expect_success 'add with directory pathspec' '
	(
	cd repo &&
	mkdir -p newdir &&
	echo "d1" >newdir/d1.txt &&
	echo "d2" >newdir/d2.txt &&
	grit add newdir &&
	grit status --porcelain >actual &&
	grep "newdir/d1.txt" actual &&
	grep "newdir/d2.txt" actual
	)
'

# -- comparison with real git ------------------------------------------------

test_expect_success 'setup: comparison repos' '
	(
	$REAL_GIT init git-cmp &&
	cd git-cmp &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "x" >f.txt &&
	$REAL_GIT add f.txt &&
	test_tick &&
	$REAL_GIT commit -m "init" &&
	cd .. &&
	grit init grit-cmp &&
	cd grit-cmp &&
	echo "x" >f.txt &&
	grit add f.txt &&
	test_tick &&
	grit commit -m "init" &&
	cd ..
	)
'

test_expect_success 'add -u stages modified in both grit and real git' '
	echo "mod" >>git-cmp/f.txt &&
	echo "mod" >>grit-cmp/f.txt &&
	$REAL_GIT -C git-cmp add -u &&
	grit -C grit-cmp add -u &&
	$REAL_GIT -C git-cmp diff --cached --name-only >expect &&
	grit -C grit-cmp diff --cached --name-only >actual &&
	test_cmp expect actual
'

test_expect_success 'add -A stages new file in both' '
	echo "new" >git-cmp/new.txt &&
	echo "new" >grit-cmp/new.txt &&
	$REAL_GIT -C git-cmp add -A &&
	grit -C grit-cmp add -A &&
	$REAL_GIT -C git-cmp diff --cached --name-only >expect &&
	grit -C grit-cmp diff --cached --name-only >actual &&
	test_cmp expect actual
'

test_done
