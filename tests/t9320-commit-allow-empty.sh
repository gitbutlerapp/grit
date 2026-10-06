#!/bin/sh
# Tests for commit --allow-empty, --allow-empty-message, --amend,
# -a/--all, --author, --date, --signoff, -F file, and error cases.

test_description='commit allow-empty, amend, signoff, author, date'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- setup ------------------------------------------------------------------

test_expect_success 'setup: init repo with initial commit' '
	(
	grit init repo &&
	cd repo &&
	echo "base" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "initial"
	)
'

# -- allow-empty -------------------------------------------------------------

test_expect_success 'commit --allow-empty succeeds with no changes' '
	(
	cd repo &&
	test_tick &&
	grit commit --allow-empty -m "empty commit"
	)
'

test_expect_success 'empty commit is recorded in log' '
	(
	cd repo &&
	grit log --oneline >actual &&
	grep "empty commit" actual
	)
'

test_expect_success 'commit without --allow-empty fails when nothing staged' '
	(
	cd repo &&
	! grit commit -m "should fail" 2>err &&
	test -s err
	)
'

test_expect_success 'multiple --allow-empty commits' '
	(
	cd repo &&
	test_tick &&
	grit commit --allow-empty -m "empty 2" &&
	test_tick &&
	grit commit --allow-empty -m "empty 3" &&
	grit log --oneline >actual &&
	grep "empty 2" actual &&
	grep "empty 3" actual
	)
'

# -- allow-empty-message -----------------------------------------------------

test_expect_success 'commit --allow-empty-message with empty message' '
	(
	cd repo &&
	echo "new content" >new.txt &&
	grit add new.txt &&
	test_tick &&
	grit commit --allow-empty-message -m ""
	)
'

test_expect_success 'commit without message fails' '
	(
	cd repo &&
	echo "more" >>new.txt &&
	grit add new.txt &&
	! grit commit 2>err &&
	test -s err
	)
'

# -- amend -------------------------------------------------------------------

test_expect_success 'commit --amend changes message' '
	(
	cd repo &&
	echo "amend-content" >amend.txt &&
	grit add amend.txt &&
	test_tick &&
	grit commit -m "before amend" &&
	test_tick &&
	grit commit --amend -m "after amend" &&
	grit log -n 1 --format=%s >actual &&
	echo "after amend" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit --amend keeps the same tree when no changes' '
	(
	cd repo &&
	grit rev-parse HEAD^{tree} >tree_before &&
	test_tick &&
	grit commit --amend -m "amend message only" &&
	grit rev-parse HEAD^{tree} >tree_after &&
	test_cmp tree_before tree_after
	)
'

test_expect_success 'commit --amend changes the commit hash' '
	(
	cd repo &&
	grit rev-parse HEAD >hash_before &&
	test_tick &&
	grit commit --amend -m "amend again" &&
	grit rev-parse HEAD >hash_after &&
	! test_cmp hash_before hash_after
	)
'

# -- -a / --all --------------------------------------------------------------

test_expect_success 'commit -a stages modified tracked files' '
	(
	cd repo &&
	echo "modified" >>file.txt &&
	test_tick &&
	grit commit -a -m "commit all" &&
	grit status --porcelain >actual &&
	! grep "file.txt" actual
	)
'

test_expect_success 'commit --all stages modified tracked files' '
	(
	cd repo &&
	echo "more mods" >>file.txt &&
	test_tick &&
	grit commit --all -m "commit all long" &&
	grit status --porcelain >actual &&
	! grep "file.txt" actual
	)
'

test_expect_success 'commit -a does not add untracked files' '
	(
	cd repo &&
	echo "untracked" >untracked.txt &&
	echo "tracked mod" >>file.txt &&
	test_tick &&
	grit commit -a -m "only tracked" &&
	grit status --porcelain >actual &&
	grep "untracked.txt" actual
	)
'

# -- --author ----------------------------------------------------------------

test_expect_success 'commit --author sets custom author' '
	(
	cd repo &&
	test_tick &&
	grit commit --allow-empty --author "Custom Author <custom@test.com>" -m "custom author" &&
	grit log -n 1 --format="%an <%ae>" >actual &&
	echo "Custom Author <custom@test.com>" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit without --author uses default author' '
	(
	cd repo &&
	test_tick &&
	grit commit --allow-empty -m "default author" &&
	grit log -n 1 --format="%an" >actual &&
	echo "Test Author" >expect &&
	test_cmp expect actual
	)
'

# -- --date ------------------------------------------------------------------

test_expect_success 'commit --date sets custom date' '
	(
	cd repo &&
	test_tick &&
	grit commit --allow-empty --date "2020-01-01T00:00:00+00:00" -m "custom date" &&
	grit cat-file -p HEAD >actual &&
	grep "author.*2020" actual || grep "1577836800" actual
	)
'

# -- --signoff flag accepted -------------------------------------------------

test_expect_success 'commit --signoff is accepted' '
	(
	cd repo &&
	test_tick &&
	grit commit --allow-empty --signoff -m "signoff commit"
	)
'

test_expect_success 'commit -s is accepted as short form' '
	(
	cd repo &&
	test_tick &&
	grit commit --allow-empty -s -m "signoff short"
	)
'

# -- -F file -----------------------------------------------------------------

test_expect_success 'commit -F reads message from file' '
	(
	cd repo &&
	echo "message from file" >msg.txt &&
	echo "more content" >>file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -F msg.txt &&
	grit log -n 1 --format=%s >actual &&
	echo "message from file" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit -F with multiline message' '
	(
	cd repo &&
	printf "subject line\n\nbody paragraph" >msg2.txt &&
	echo "yet more" >>file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -F msg2.txt &&
	grit log -n 1 --format=%s >actual &&
	echo "subject line" >expect &&
	test_cmp expect actual
	)
'

# -- quiet flag --------------------------------------------------------------

test_expect_success 'commit -q suppresses output' '
	(
	cd repo &&
	echo "quiet" >>file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -q -m "quiet commit" >actual 2>&1 &&
	test_must_be_empty actual
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

test_expect_success 'allow-empty creates commit in both' '
	$REAL_GIT -C git-cmp commit --allow-empty -m "empty" &&
	grit -C grit-cmp commit --allow-empty -m "empty" &&
	$REAL_GIT -C git-cmp log --oneline >expect &&
	grit -C grit-cmp log --oneline >actual &&
	test "$(wc -l <expect)" = "$(wc -l <actual)"
'

test_expect_success 'amend in both changes subject' '
	$REAL_GIT -C git-cmp commit --amend --allow-empty -m "amended" &&
	grit -C grit-cmp commit --amend -m "amended" &&
	$REAL_GIT -C git-cmp log -1 --format=%s >expect &&
	grit -C grit-cmp log -n 1 --format=%s >actual &&
	test_cmp expect actual
'

test_done
