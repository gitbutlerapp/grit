#!/bin/sh
# Tests for checkout file restore and the restore command.

test_description='checkout file restore and restore command'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- setup ---------------------------------------------------------------------

test_expect_success 'setup: create repo with commits' '
	(
	git init restore-repo &&
	cd restore-repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "original" >file.txt &&
	echo "keep" >keep.txt &&
	git add file.txt keep.txt &&
	test_tick &&
	git commit -m "initial" &&
	echo "v2" >file.txt &&
	echo "extra" >extra.txt &&
	git add file.txt extra.txt &&
	test_tick &&
	git commit -m "second"
	)
'

# -- checkout -- file restores from index --------------------------------------

test_expect_success 'checkout -- file restores modified file from index' '
	(
	cd restore-repo &&
	echo "dirty" >file.txt &&
	git checkout -- file.txt &&
	echo "v2" >expect &&
	test_cmp expect file.txt
	)
'

test_expect_success 'checkout -- file does not affect other files' '
	(
	cd restore-repo &&
	echo "keep" >expect &&
	test_cmp expect keep.txt
	)
'

test_expect_success 'checkout -- multiple files' '
	(
	cd restore-repo &&
	echo "dirty1" >file.txt &&
	echo "dirty2" >keep.txt &&
	git checkout -- file.txt keep.txt &&
	echo "v2" >expect-file &&
	echo "keep" >expect-keep &&
	test_cmp expect-file file.txt &&
	test_cmp expect-keep keep.txt
	)
'

# -- restore command (worktree) ------------------------------------------------

test_expect_success 'restore file from index (default)' '
	(
	cd restore-repo &&
	echo "dirty" >file.txt &&
	git restore file.txt &&
	echo "v2" >expect &&
	test_cmp expect file.txt
	)
'

test_expect_success 'restore --worktree restores from index' '
	(
	cd restore-repo &&
	echo "dirty" >file.txt &&
	git restore --worktree file.txt &&
	echo "v2" >expect &&
	test_cmp expect file.txt
	)
'

test_expect_success 'restore with dot restores all tracked files' '
	(
	cd restore-repo &&
	echo "dirty1" >file.txt &&
	echo "dirty2" >keep.txt &&
	git restore . &&
	echo "v2" >expect-file &&
	echo "keep" >expect-keep &&
	test_cmp expect-file file.txt &&
	test_cmp expect-keep keep.txt
	)
'

# -- restore --staged (unstage) ------------------------------------------------

test_expect_success 'restore --staged unstages a file' '
	(
	cd restore-repo &&
	echo "staged-content" >staged.txt &&
	git add staged.txt &&
	git diff --cached --name-only >before &&
	grep "staged.txt" before &&
	git restore --staged staged.txt &&
	git diff --cached --name-only >after &&
	! grep "staged.txt" after
	)
'

test_expect_success 'restore --staged keeps file in worktree' '
	(
	cd restore-repo &&
	test -f staged.txt &&
	echo "staged-content" >expect &&
	test_cmp expect staged.txt
	)
'

test_expect_success 'restore --staged on modified tracked file' '
	(
	cd restore-repo &&
	echo "modified" >file.txt &&
	git add file.txt &&
	git restore --staged file.txt &&
	git diff --cached --name-only >staged &&
	! grep "file.txt" staged
	)
'

test_expect_success 'restore --staged preserves worktree modification' '
	(
	cd restore-repo &&
	echo "modified" >expect &&
	test_cmp expect file.txt
	)
'

# -- restore --source (from specific commit) -----------------------------------

test_expect_success 'restore --source HEAD restores committed version' '
	(
	cd restore-repo &&
	echo "dirty-source" >file.txt &&
	git restore --source HEAD -- file.txt &&
	echo "v2" >expect &&
	test_cmp expect file.txt
	)
'

test_expect_success 'restore --source HEAD works on multiple files' '
	(
	cd restore-repo &&
	echo "dirty1" >file.txt &&
	echo "dirty2" >keep.txt &&
	git restore --source HEAD -- file.txt keep.txt &&
	echo "v2" >expect-file &&
	echo "keep" >expect-keep &&
	test_cmp expect-file file.txt &&
	test_cmp expect-keep keep.txt
	)
'

test_expect_success 'restore --source HEAD restores current version' '
	(
	cd restore-repo &&
	echo "dirty" >file.txt &&
	git restore --source HEAD file.txt &&
	echo "v2" >expect &&
	test_cmp expect file.txt
	)
'

# -- checkout to specific commit for file -------------------------------------

test_expect_success 'restore --staged after add restores index to HEAD' '
	(
	cd restore-repo &&
	echo "modified" >file.txt &&
	git add file.txt &&
	git restore --staged -- file.txt &&
	git diff --cached --name-only >staged &&
	! grep "file.txt" staged
	)
'

test_expect_success 'restore file back to HEAD version in worktree' '
	(
	cd restore-repo &&
	git restore --source HEAD -- file.txt &&
	echo "v2" >expect &&
	test_cmp expect file.txt
	)
'

test_expect_success 'restore --staged resets index to HEAD' '
	(
	cd restore-repo &&
	git restore --staged -- file.txt &&
	git diff --cached --name-only >staged &&
	! grep "file.txt" staged
	)
'

# -- checkout/restore error cases ----------------------------------------------

test_expect_success 'checkout -- nonexistent file fails' '
	(
	cd restore-repo &&
	test_expect_code 1 git checkout -- nonexistent.txt 2>/dev/null
	)
'

test_expect_success 'restore nonexistent file fails' '
	(
	cd restore-repo &&
	test_expect_code 1 git restore nonexistent.txt 2>/dev/null
	)
'

# -- restore after deletion ---------------------------------------------------

test_expect_success 'restore recovers deleted tracked file' '
	(
	cd restore-repo &&
	rm file.txt &&
	! test -f file.txt &&
	git restore file.txt &&
	test -f file.txt &&
	echo "v2" >expect &&
	test_cmp expect file.txt
	)
'

test_expect_success 'checkout -- recovers deleted tracked file' '
	(
	cd restore-repo &&
	rm keep.txt &&
	! test -f keep.txt &&
	git checkout -- keep.txt &&
	test -f keep.txt &&
	echo "keep" >expect &&
	test_cmp expect keep.txt
	)
'

# -- restore multiple files at once -------------------------------------------

test_expect_success 'restore multiple files at once' '
	(
	cd restore-repo &&
	echo "dirty1" >file.txt &&
	echo "dirty2" >extra.txt &&
	git restore file.txt extra.txt &&
	echo "v2" >expect-file &&
	echo "extra" >expect-extra &&
	test_cmp expect-file file.txt &&
	test_cmp expect-extra extra.txt
	)
'

# -- checkout branch vs file disambiguation -----------------------------------

test_expect_success 'restore works when branch with same name exists' '
	(
	cd restore-repo &&
	git branch same-as-file 2>/dev/null || true &&
	echo "dirty" >extra.txt &&
	git restore extra.txt &&
	echo "extra" >expect &&
	test_cmp expect extra.txt
	)
'

# -- restore --quiet -----------------------------------------------------------

test_expect_success 'restore --quiet suppresses output' '
	(
	cd restore-repo &&
	echo "dirty" >file.txt &&
	git restore --quiet file.txt >out 2>&1 &&
	test_must_be_empty out
	)
'

test_expect_success 'restore after staging new content restores from index' '
	(
	cd restore-repo &&
	echo "new-staged" >file.txt &&
	git add file.txt &&
	echo "dirty-again" >file.txt &&
	git restore file.txt &&
	echo "new-staged" >expect &&
	test_cmp expect file.txt
	)
'

test_done
