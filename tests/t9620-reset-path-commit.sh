#!/bin/sh
# Tests for grit reset: --soft, --mixed (default), --hard, path-based reset,
# reset to specific commits, -q (quiet), and cross-checks with real git.

test_description='grit reset --soft, --mixed, --hard, path, commit'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# Helper: strip ## header from porcelain
grit_status_clean () {
	grit status --porcelain | grep -v "^##"
}

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with three commits' '
	(
	grit init repo &&
	cd repo &&
	echo "v1" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "commit-1" &&
	echo "v2" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "commit-2" &&
	echo "v3" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "commit-3"
	)
'

###########################################################################
# Section 2: --soft
###########################################################################

test_expect_success 'reset --soft moves HEAD but keeps index and worktree' '
	(
	cd repo &&
	prev=$(grit rev-parse HEAD~1) &&
	grit reset --soft HEAD~1 &&
	test "$(grit rev-parse HEAD)" = "$prev"
	)
'

test_expect_success 'reset --soft: index still has new content' '
	(
	cd repo &&
	grit_status_clean >actual &&
	grep "^M  file.txt" actual
	)
'

test_expect_success 'reset --soft: worktree has v3 content' '
	(
	cd repo &&
	grep "v3" file.txt
	)
'

test_expect_success 'restore to commit-3 for next tests' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "re-commit-3"
	)
'

###########################################################################
# Section 3: --mixed (default)
###########################################################################

test_expect_success 'reset --mixed moves HEAD and resets index' '
	(
	cd repo &&
	prev=$(grit rev-parse HEAD~1) &&
	grit reset --mixed HEAD~1 &&
	test "$(grit rev-parse HEAD)" = "$prev"
	)
'

test_expect_success 'reset --mixed: changes are unstaged' '
	(
	cd repo &&
	grit_status_clean >actual &&
	grep "^ M file.txt" actual
	)
'

test_expect_success 'reset --mixed: worktree still has v3' '
	(
	cd repo &&
	grep "v3" file.txt
	)
'

test_expect_success 'restore state' '
	(
	cd repo &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "re-commit-3b"
	)
'

test_expect_success 'reset without mode flag defaults to --mixed' '
	(
	cd repo &&
	prev=$(grit rev-parse HEAD~1) &&
	grit reset HEAD~1 &&
	test "$(grit rev-parse HEAD)" = "$prev" &&
	grit_status_clean >actual &&
	grep "^ M file.txt" actual
	)
'

test_expect_success 'restore state again' '
	(
	cd repo &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "re-commit-3c"
	)
'

###########################################################################
# Section 4: --hard
###########################################################################

test_expect_success 'reset --hard moves HEAD, index, and worktree' '
	(
	cd repo &&
	prev=$(grit rev-parse HEAD~1) &&
	grit reset --hard HEAD~1 &&
	test "$(grit rev-parse HEAD)" = "$prev"
	)
'

test_expect_success 'reset --hard: working tree matches target commit' '
	(
	cd repo &&
	grep "v2" file.txt
	)
'

test_expect_success 'reset --hard: status is clean' '
	(
	cd repo &&
	grit_status_clean >actual &&
	! grep "file.txt" actual
	)
'

test_expect_success 'reset --hard back to original state' '
	(
	cd repo &&
	echo "v3" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "re-v3"
	)
'

###########################################################################
# Section 5: Path-based reset
###########################################################################

test_expect_success 'reset HEAD -- path unstages a file' '
	(
	cd repo &&
	echo "new-content" >file.txt &&
	grit add file.txt &&
	grit reset HEAD -- file.txt &&
	grit_status_clean >actual &&
	grep "^ M file.txt" actual
	)
'

test_expect_success 'reset path does not move HEAD' '
	(
	cd repo &&
	head_before=$(grit rev-parse HEAD) &&
	grit add file.txt &&
	grit reset HEAD -- file.txt &&
	head_after=$(grit rev-parse HEAD) &&
	test "$head_before" = "$head_after"
	)
'

test_expect_success 'reset path: worktree unchanged' '
	(
	cd repo &&
	grep "new-content" file.txt
	)
'

test_expect_success 'clean up path reset' '
	(
	cd repo &&
	grit restore file.txt &&
	grep "v3" file.txt
	)
'

test_expect_success 'reset with specific commit and path' '
	(
	cd repo &&
	echo "modified" >file.txt &&
	grit add file.txt &&
	ancestor=$(grit rev-parse HEAD~1) &&
	grit reset $ancestor -- file.txt &&
	grit ls-files --stage >actual &&
	grep "file.txt" actual
	)
'

test_expect_success 'clean up' '
	(
	cd repo &&
	grit restore --staged file.txt &&
	grit restore file.txt
	)
'

###########################################################################
# Section 6: Reset with multiple files
###########################################################################

test_expect_success 'setup: add more files' '
	(
	cd repo &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	grit add a.txt b.txt &&
	test_tick &&
	grit commit -m "add a and b"
	)
'

test_expect_success 'reset HEAD -- with multiple paths' '
	(
	cd repo &&
	echo "mod-a" >a.txt &&
	echo "mod-b" >b.txt &&
	grit add a.txt b.txt &&
	grit reset HEAD -- a.txt b.txt &&
	grit_status_clean >actual &&
	grep "^ M a.txt" actual &&
	grep "^ M b.txt" actual
	)
'

test_expect_success 'clean up multi-file' '
	(
	cd repo &&
	grit restore a.txt b.txt
	)
'

###########################################################################
# Section 7: --quiet
###########################################################################

test_expect_success 'reset -q suppresses output' '
	(
	cd repo &&
	echo "x" >file.txt &&
	grit add file.txt &&
	grit reset -q HEAD -- file.txt 2>err &&
	test_must_be_empty err
	)
'

test_expect_success 'reset --quiet same as -q' '
	(
	cd repo &&
	grit add file.txt &&
	grit reset --quiet HEAD -- file.txt 2>err &&
	test_must_be_empty err
	)
'

test_expect_success 'clean up' '
	(
	cd repo &&
	grit restore file.txt
	)
'

###########################################################################
# Section 8: Edge cases
###########################################################################

test_expect_success 'save tip ref before reset' '
	(
	cd repo &&
	grit rev-parse HEAD >"$TRASH_DIRECTORY/saved-tip"
	)
'

test_expect_success 'reset --hard to older commit' '
	(
	cd repo &&
	prev=$(grit rev-parse HEAD~1) &&
	grit reset --hard $prev &&
	test "$(grit rev-parse HEAD)" = "$prev"
	)
'

test_expect_success 'restore to tip' '
	(
	cd repo &&
	tip=$(cat "$TRASH_DIRECTORY/saved-tip") &&
	grit reset --hard $tip &&
	grep "v3" file.txt
	)
'

test_expect_success 'reset --soft to current HEAD is no-op' '
	(
	cd repo &&
	head_before=$(grit rev-parse HEAD) &&
	grit reset --soft HEAD &&
	head_after=$(grit rev-parse HEAD) &&
	test "$head_before" = "$head_after"
	)
'

test_expect_success 'reset --mixed to current HEAD is no-op' '
	(
	cd repo &&
	head_before=$(grit rev-parse HEAD) &&
	grit reset --mixed HEAD &&
	head_after=$(grit rev-parse HEAD) &&
	test "$head_before" = "$head_after"
	)
'

###########################################################################
# Section 9: Cross-check with real git
###########################################################################

test_expect_success 'setup cross-check repos' '
	(
	$REAL_GIT init git-repo &&
	cd git-repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "v1" >f.txt &&
	$REAL_GIT add f.txt &&
	test_tick &&
	$REAL_GIT commit -m "c1" &&
	echo "v2" >f.txt &&
	$REAL_GIT add f.txt &&
	test_tick &&
	$REAL_GIT commit -m "c2" &&
	cd .. &&
	grit init grit-repo &&
	cd grit-repo &&
	echo "v1" >f.txt &&
	grit add f.txt &&
	test_tick &&
	grit commit -m "c1" &&
	echo "v2" >f.txt &&
	grit add f.txt &&
	test_tick &&
	grit commit -m "c2"
	)
'

test_expect_success 'reset --hard HEAD~1 matches real git' '
	$REAL_GIT -C git-repo reset --hard HEAD~1 &&
	grit -C grit-repo reset --hard HEAD~1 &&
	diff git-repo/f.txt grit-repo/f.txt
'

test_expect_success 'reset --soft HEAD is no-op in cross-check' '
	git_head=$($REAL_GIT -C git-repo rev-parse HEAD) &&
	$REAL_GIT -C git-repo reset --soft HEAD &&
	grit_head=$(grit -C grit-repo rev-parse HEAD) &&
	grit -C grit-repo reset --soft HEAD &&
	test "$($REAL_GIT -C git-repo rev-parse HEAD)" = "$git_head" &&
	test "$(grit -C grit-repo rev-parse HEAD)" = "$grit_head"
'

test_expect_success 'reset --mixed preserves worktree in cross-check' '
	echo "v2" >git-repo/f.txt &&
	echo "v2" >grit-repo/f.txt &&
	$REAL_GIT -C git-repo add f.txt &&
	grit -C grit-repo add f.txt &&
	test_tick &&
	$REAL_GIT -C git-repo commit -m "c2b" &&
	grit -C grit-repo commit -m "c2b" &&
	$REAL_GIT -C git-repo reset HEAD~1 &&
	grit -C grit-repo reset HEAD~1 &&
	diff git-repo/f.txt grit-repo/f.txt
'

test_expect_success 'reset path in cross-check' '
	$REAL_GIT -C git-repo add f.txt &&
	grit -C grit-repo add f.txt &&
	$REAL_GIT -C git-repo reset HEAD -- f.txt &&
	grit -C grit-repo reset HEAD -- f.txt &&
	$REAL_GIT -C git-repo diff --name-only >expect &&
	grit -C grit-repo diff --name-only >actual &&
	test_cmp expect actual
'

test_expect_success 'reset --hard HEAD in cross-check' '
	$REAL_GIT -C git-repo reset --hard HEAD &&
	grit -C grit-repo reset --hard HEAD &&
	diff git-repo/f.txt grit-repo/f.txt
'

test_expect_success 'ls-files matches after reset --hard' '
	$REAL_GIT -C git-repo ls-files >expect &&
	grit -C grit-repo ls-files >actual &&
	test_cmp expect actual
'

test_expect_success 'file content matches after all resets' '
	diff git-repo/f.txt grit-repo/f.txt
'

test_done
