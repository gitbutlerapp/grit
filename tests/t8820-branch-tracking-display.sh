#!/bin/sh
# Tests for branch listing, creation, deletion, renaming, and verbose display.

test_description='branch listing, tracking display, and management'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- setup ---------------------------------------------------------------------

test_expect_success 'setup: init repo with initial commit' '
	(
	git init branch-repo &&
	cd branch-repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "base" >base.txt &&
	git add base.txt &&
	test_tick &&
	git commit -m "initial commit"
	)
'

# -- branch creation -----------------------------------------------------------

test_expect_success 'create a new branch' '
	(
	cd branch-repo &&
	git branch feature-a &&
	git branch >out &&
	grep "feature-a" out
	)
'

test_expect_success 'create branch does not switch to it' '
	(
	cd branch-repo &&
	git branch >out &&
	grep "^\*" out >current &&
	! grep "feature-a" current
	)
'

test_expect_success 'create multiple branches' '
	(
	cd branch-repo &&
	git branch feature-b &&
	git branch feature-c &&
	git branch >out &&
	grep "feature-a" out &&
	grep "feature-b" out &&
	grep "feature-c" out
	)
'

test_expect_success 'branch --list shows all branches' '
	(
	cd branch-repo &&
	git branch --list >out &&
	grep "feature-a" out &&
	grep "feature-b" out &&
	grep "feature-c" out
	)
'

test_expect_success 'create branch at specific commit' '
	(
	cd branch-repo &&
	echo "second" >second.txt &&
	git add second.txt &&
	test_tick &&
	git commit -m "second commit" &&
	first=$(git rev-parse HEAD~1) &&
	git branch at-first "$first" &&
	git rev-parse at-first >out &&
	echo "$first" >expect &&
	test_cmp expect out
	)
'

# -- branch listing ------------------------------------------------------------

test_expect_success 'branch lists current branch with asterisk' '
	(
	cd branch-repo &&
	git branch >out &&
	grep "^\*" out
	)
'

test_expect_success 'branch -v shows commit hash and subject' '
	(
	cd branch-repo &&
	git branch -v >out &&
	grep "second commit" out
	)
'

test_expect_success 'branch -v shows abbreviated hash' '
	(
	cd branch-repo &&
	git branch -v >out &&
	full=$(git rev-parse HEAD) &&
	short=$(echo "$full" | cut -c1-7) &&
	grep "$short" out
	)
'

# -- checkout/switch branches -------------------------------------------------

test_expect_success 'checkout branch changes HEAD' '
	(
	cd branch-repo &&
	git checkout feature-a &&
	git branch >out &&
	grep "^\* feature-a" out
	)
'

test_expect_success 'checkout back to original branch' '
	(
	cd branch-repo &&
	git checkout master &&
	git branch >out &&
	grep "^\* master" out
	)
'

test_expect_success 'checkout -b creates and switches' '
	(
	cd branch-repo &&
	git checkout -b feature-d &&
	git branch >out &&
	grep "^\* feature-d" out
	)
'

# -- branch deletion ----------------------------------------------------------

test_expect_success 'delete a merged branch' '
	(
	cd branch-repo &&
	git checkout master &&
	git branch -d feature-d &&
	git branch >out &&
	! grep "feature-d" out
	)
'

test_expect_success 'delete branch not on it' '
	(
	cd branch-repo &&
	git branch temp-del &&
	git branch -d temp-del &&
	git branch >out &&
	! grep "temp-del" out
	)
'

test_expect_success 'cannot delete current branch' '
	(
	cd branch-repo &&
	test_expect_code 1 git branch -d master
	)
'

test_expect_success 'force-delete unmerged branch with -D' '
	(
	cd branch-repo &&
	git checkout -b unmerged-branch &&
	echo "unmerged content" >unmerged.txt &&
	git add unmerged.txt &&
	test_tick &&
	git commit -m "unmerged work" &&
	git checkout master &&
	git branch -D unmerged-branch &&
	git branch >out &&
	! grep "unmerged-branch" out
	)
'

# -- branch rename -------------------------------------------------------------

test_expect_success 'rename a branch with -m' '
	(
	cd branch-repo &&
	git branch rename-me &&
	git branch -m rename-me renamed &&
	git branch >out &&
	grep "renamed" out &&
	! grep "rename-me" out
	)
'

test_expect_success 'rename current branch with two-arg form' '
	(
	cd branch-repo &&
	git checkout -b old-name &&
	git branch -m old-name new-name &&
	git branch >out &&
	grep "new-name" out &&
	! grep "old-name" out
	)
'

test_expect_success 'cleanup: return to master' '
	(
	cd branch-repo &&
	git checkout master
	)
'

# -- branch at various refs ----------------------------------------------------

test_expect_success 'create branch from resolved parent commit' '
	(
	cd branch-repo &&
	parent=$(git rev-parse HEAD~1) &&
	git branch from-parent "$parent" &&
	git rev-parse from-parent >out &&
	echo "$parent" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'create branch from tag' '
	(
	cd branch-repo &&
	git tag v1.0 HEAD &&
	git branch from-tag v1.0 &&
	tagged=$(git rev-parse v1.0) &&
	git rev-parse from-tag >out &&
	echo "$tagged" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'branch -a lists all branches' '
	(
	cd branch-repo &&
	git branch -a >out &&
	grep "feature-a" out
	)
'

# -- branch contains / no-contains (if supported) -----------------------------

test_expect_success 'branch --contains HEAD shows current branches' '
	(
	cd branch-repo &&
	git branch --contains HEAD >out &&
	grep "master" out
	)
'

test_expect_success 'branch --contains HEAD does not show detached branches' '
	(
	cd branch-repo &&
	git branch --contains HEAD >out &&
	line_count=$(wc -l <out) &&
	test "$line_count" -ge 1
	)
'

test_expect_success 'branch --contains with older commit shows more' '
	(
	cd branch-repo &&
	first=$(git rev-parse HEAD~1) &&
	git branch --contains "$first" >out &&
	grep "master" out
	)
'

# -- error cases ---------------------------------------------------------------

test_expect_success 'cannot create branch with invalid name' '
	(
	cd branch-repo &&
	test_expect_code 1 git branch ".." 2>/dev/null
	)
'

test_expect_success 'cannot create duplicate branch' '
	(
	cd branch-repo &&
	test_expect_code 1 git branch feature-a 2>/dev/null
	)
'

test_done
