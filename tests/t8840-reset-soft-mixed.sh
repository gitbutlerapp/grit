#!/bin/sh
# Tests for git reset --soft, --mixed, and --hard.

test_description='reset soft, mixed, and hard modes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- setup ---------------------------------------------------------------------

test_expect_success 'setup: create repo with three commits' '
	(
	git init reset-repo &&
	cd reset-repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "first" >file.txt &&
	git add file.txt &&
	test_tick &&
	git commit -m "commit-1" &&
	echo "second" >file2.txt &&
	git add file2.txt &&
	test_tick &&
	git commit -m "commit-2" &&
	echo "third" >file3.txt &&
	git add file3.txt &&
	test_tick &&
	git commit -m "commit-3"
	)
'

test_expect_success 'setup: record commit hashes' '
	(
	cd reset-repo &&
	git log --format=%H --reverse >all-hashes &&
	commit1=$(sed -n 1p all-hashes) &&
	commit2=$(sed -n 2p all-hashes) &&
	commit3=$(sed -n 3p all-hashes) &&
	test -n "$commit1" &&
	test -n "$commit2" &&
	test -n "$commit3"
	)
'

# -- reset --soft --------------------------------------------------------------

test_expect_success 'reset --soft moves HEAD but keeps index and worktree' '
	(
	cd reset-repo &&
	commit2=$(git log --format=%H --reverse | sed -n 2p) &&
	git reset --soft "$commit2" &&
	head_now=$(git rev-parse HEAD) &&
	test "$head_now" = "$commit2"
	)
'

test_expect_success 'after soft reset, file3.txt is still staged' '
	(
	cd reset-repo &&
	git diff --cached --name-only >out &&
	grep "file3.txt" out
	)
'

test_expect_success 'after soft reset, file3.txt exists in worktree' '
	(
	cd reset-repo &&
	test -f file3.txt &&
	echo "third" >expect &&
	test_cmp expect file3.txt
	)
'

test_expect_success 'after soft reset, commit restores to original state' '
	(
	cd reset-repo &&
	test_tick &&
	git commit -m "re-commit-3" &&
	git log --format=%s -n 1 >out &&
	echo "re-commit-3" >expect &&
	test_cmp expect out
	)
'

# -- reset --mixed (default) --------------------------------------------------

test_expect_success 'setup: create more commits for mixed reset' '
	(
	cd reset-repo &&
	echo "four" >file4.txt &&
	git add file4.txt &&
	test_tick &&
	git commit -m "commit-4" &&
	echo "five" >file5.txt &&
	git add file5.txt &&
	test_tick &&
	git commit -m "commit-5"
	)
'

test_expect_success 'reset --mixed moves HEAD and unstages changes' '
	(
	cd reset-repo &&
	target=$(git log --format=%H --skip=1 -n 1) &&
	git reset --mixed "$target" &&
	head_now=$(git rev-parse HEAD) &&
	test "$head_now" = "$target"
	)
'

test_expect_success 'after mixed reset, file5.txt is untracked/unstaged' '
	(
	cd reset-repo &&
	git diff --cached --name-only >staged &&
	! grep "file5.txt" staged
	)
'

test_expect_success 'after mixed reset, file5.txt still exists in worktree' '
	(
	cd reset-repo &&
	test -f file5.txt &&
	echo "five" >expect &&
	test_cmp expect file5.txt
	)
'

test_expect_success 'default reset (no mode) is same as --mixed' '
	(
	cd reset-repo &&
	git add file5.txt &&
	test_tick &&
	git commit -m "commit-5-again" &&
	target=$(git log --format=%H --skip=1 -n 1) &&
	git reset "$target" &&
	head_now=$(git rev-parse HEAD) &&
	test "$head_now" = "$target" &&
	test -f file5.txt
	)
'

# -- reset --hard --------------------------------------------------------------

test_expect_success 'setup: recommit for hard reset test' '
	(
	cd reset-repo &&
	git add file5.txt &&
	test_tick &&
	git commit -m "commit-5-final" &&
	echo "six" >file6.txt &&
	git add file6.txt &&
	test_tick &&
	git commit -m "commit-6"
	)
'

test_expect_success 'reset --hard moves HEAD and cleans worktree' '
	(
	cd reset-repo &&
	target=$(git log --format=%H --skip=1 -n 1) &&
	git reset --hard "$target" &&
	head_now=$(git rev-parse HEAD) &&
	test "$head_now" = "$target"
	)
'

test_expect_success 'after hard reset, file6.txt is gone from worktree' '
	(
	cd reset-repo &&
	! test -f file6.txt
	)
'

test_expect_success 'after hard reset, index is clean' '
	(
	cd reset-repo &&
	git diff --cached --name-only >staged &&
	test_must_be_empty staged
	)
'

# -- reset to HEAD -------------------------------------------------------------

test_expect_success 'reset HEAD unstages staged changes' '
	(
	cd reset-repo &&
	echo "new content" >new-file.txt &&
	git add new-file.txt &&
	git diff --cached --name-only >before &&
	grep "new-file.txt" before &&
	git reset HEAD &&
	git diff --cached --name-only >after &&
	! grep "new-file.txt" after
	)
'

test_expect_success 'reset HEAD preserves working tree' '
	(
	cd reset-repo &&
	test -f new-file.txt &&
	echo "new content" >expect &&
	test_cmp expect new-file.txt
	)
'

# -- reset with paths ----------------------------------------------------------

test_expect_success 'reset HEAD -- path unstages single file' '
	(
	cd reset-repo &&
	echo "a" >path-a.txt &&
	echo "b" >path-b.txt &&
	git add path-a.txt path-b.txt &&
	git reset HEAD -- path-a.txt &&
	git diff --cached --name-only >staged &&
	! grep "path-a.txt" staged &&
	grep "path-b.txt" staged
	)
'

test_expect_success 'reset with path preserves file in worktree' '
	(
	cd reset-repo &&
	test -f path-a.txt &&
	echo "a" >expect &&
	test_cmp expect path-a.txt
	)
'

# -- reset --soft to same commit (no-op) --------------------------------------

test_expect_success 'reset --soft HEAD is a no-op' '
	(
	cd reset-repo &&
	head_before=$(git rev-parse HEAD) &&
	git reset --soft HEAD &&
	head_after=$(git rev-parse HEAD) &&
	test "$head_before" = "$head_after"
	)
'

# -- reset and branch tip ------------------------------------------------------

test_expect_success 'reset moves branch tip' '
	(
	cd reset-repo &&
	git add path-b.txt &&
	test_tick &&
	git commit -m "path-b commit" &&
	new_head=$(git rev-parse HEAD) &&
	old_head=$(git log --format=%H --skip=1 -n 1) &&
	git reset --soft "$old_head" &&
	branch_tip=$(git rev-parse HEAD) &&
	test "$branch_tip" = "$old_head"
	)
'

test_expect_success 'after reset, re-commit creates new history' '
	(
	cd reset-repo &&
	test_tick &&
	git commit -m "new path-b commit" &&
	git log --format=%s -n 1 >out &&
	echo "new path-b commit" >expect &&
	test_cmp expect out
	)
'

# -- reset --hard with dirty worktree -----------------------------------------

test_expect_success 'reset --hard discards dirty worktree changes' '
	(
	cd reset-repo &&
	echo "dirty" >>file.txt &&
	git reset --hard HEAD &&
	echo "first" >expect &&
	test_cmp expect file.txt
	)
'

test_expect_success 'reset --hard removes untracked files added to index' '
	(
	cd reset-repo &&
	echo "temp" >temp-file.txt &&
	git add temp-file.txt &&
	git reset --hard HEAD &&
	! test -f temp-file.txt
	)
'

# -- quiet mode ----------------------------------------------------------------

test_expect_success 'reset --quiet suppresses output' '
	(
	cd reset-repo &&
	echo "q" >quiet-file.txt &&
	git add quiet-file.txt &&
	test_tick &&
	git commit -m "quiet test" &&
	target=$(git log --format=%H --skip=1 -n 1) &&
	git reset --quiet --soft "$target" >out 2>&1 &&
	test_must_be_empty out
	)
'

test_done
