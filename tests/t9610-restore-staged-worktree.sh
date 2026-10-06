#!/bin/sh
# Tests for grit restore: --staged (unstage), --worktree (restore working tree),
# --source, combined --staged --worktree, pathspecs, and cross-checks.

test_description='grit restore --staged, --worktree, --source'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# Helper: grit status --porcelain includes "## branch" header; strip it.
grit_status_clean () {
	grit status --porcelain | grep -v "^##"
}

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with committed files' '
	(
	grit init repo &&
	cd repo &&
	echo "original alpha" >alpha.txt &&
	echo "original beta" >beta.txt &&
	mkdir -p sub &&
	echo "original sub" >sub/file.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "initial commit"
	)
'

###########################################################################
# Section 2: --staged (unstage changes)
###########################################################################

test_expect_success 'restore --staged unstages a newly added file' '
	(
	cd repo &&
	echo "new" >new.txt &&
	grit add new.txt &&
	grit restore --staged new.txt &&
	grit_status_clean >actual &&
	grep "^?? new.txt" actual
	)
'

test_expect_success 'restore --staged unstages modification' '
	(
	cd repo &&
	echo "modified" >>alpha.txt &&
	grit add alpha.txt &&
	grit restore --staged alpha.txt &&
	grit_status_clean >actual &&
	grep "^ M alpha.txt" actual
	)
'

test_expect_success 'restore --staged with multiple files' '
	(
	cd repo &&
	grit add alpha.txt new.txt &&
	grit restore --staged alpha.txt new.txt &&
	grit_status_clean >actual &&
	grep "^ M alpha.txt" actual &&
	grep "^?? new.txt" actual
	)
'

test_expect_success 'restore --staged does not modify working tree' '
	(
	cd repo &&
	grep "modified" alpha.txt
	)
'

test_expect_success 'restore -S is same as --staged' '
	(
	cd repo &&
	grit add alpha.txt &&
	grit restore -S alpha.txt &&
	grit_status_clean >actual &&
	grep "^ M alpha.txt" actual
	)
'

test_expect_success 'restore --staged on deleted file re-adds to index' '
	(
	cd repo &&
	grit restore alpha.txt &&
	rm -f new.txt &&
	rm beta.txt &&
	grit add beta.txt &&
	grit restore --staged beta.txt &&
	grit ls-files >actual &&
	grep "beta.txt" actual
	)
'

###########################################################################
# Section 3: --worktree (restore working tree from index)
###########################################################################

test_expect_success 'restore --worktree reverts modified file' '
	(
	cd repo &&
	grit restore beta.txt &&
	echo "dirty" >alpha.txt &&
	grit restore --worktree alpha.txt &&
	grit_status_clean >actual &&
	! grep "alpha.txt" actual
	)
'

test_expect_success 'restore -W is same as --worktree' '
	(
	cd repo &&
	echo "dirty again" >beta.txt &&
	grit restore -W beta.txt &&
	grep "original beta" beta.txt
	)
'

test_expect_success 'restore without flags defaults to --worktree' '
	(
	cd repo &&
	echo "more dirt" >alpha.txt &&
	grit restore alpha.txt &&
	grep "original alpha" alpha.txt
	)
'

test_expect_success 'restore --worktree on deleted file restores it' '
	(
	cd repo &&
	rm alpha.txt &&
	grit restore --worktree alpha.txt &&
	test -f alpha.txt &&
	grep "original alpha" alpha.txt
	)
'

test_expect_success 'restore --worktree with specific sub-path' '
	(
	cd repo &&
	echo "dirty sub" >sub/file.txt &&
	grit restore sub/file.txt &&
	grep "original sub" sub/file.txt
	)
'

test_expect_success 'restore dot restores all modified files' '
	(
	cd repo &&
	echo "dirty1" >alpha.txt &&
	echo "dirty2" >beta.txt &&
	grit restore . &&
	grit_status_clean >actual &&
	! grep "alpha.txt" actual &&
	! grep "beta.txt" actual
	)
'

###########################################################################
# Section 4: --source (restore from specific commit)
###########################################################################

test_expect_success 'setup: create second and third commits' '
	(
	cd repo &&
	echo "v2 alpha" >alpha.txt &&
	grit add alpha.txt &&
	test_tick &&
	grit commit -m "v2 alpha" &&
	echo "v3 alpha" >alpha.txt &&
	grit add alpha.txt &&
	test_tick &&
	grit commit -m "v3 alpha"
	)
'

test_expect_success 'restore --source HEAD~1 restores old version to worktree' '
	(
	cd repo &&
	prev=$(grit rev-parse HEAD~1) &&
	grit restore --source $prev alpha.txt &&
	grep "v2 alpha" alpha.txt
	)
'

test_expect_success 'restore -s is same as --source' '
	(
	cd repo &&
	grit restore -s HEAD alpha.txt &&
	grep "v3 alpha" alpha.txt
	)
'

test_expect_success 'restore --source HEAD~2 goes back two commits' '
	(
	cd repo &&
	ancestor=$(grit rev-parse HEAD~2) &&
	grit restore --source $ancestor alpha.txt &&
	grep "original alpha" alpha.txt
	)
'

test_expect_success 'restore from HEAD to clean up' '
	(
	cd repo &&
	grit restore -s HEAD alpha.txt &&
	grep "v3 alpha" alpha.txt
	)
'

test_expect_success 'restore --source with --staged restores index from commit' '
	(
	cd repo &&
	echo "staged change" >alpha.txt &&
	grit add alpha.txt &&
	grit restore --source HEAD --staged alpha.txt &&
	grit_status_clean >actual &&
	grep "alpha.txt" actual
	)
'

test_expect_success 'clean up after source-staged test' '
	(
	cd repo &&
	grit restore alpha.txt &&
	grep "v3 alpha" alpha.txt
	)
'

###########################################################################
# Section 5: Combined --staged --worktree
###########################################################################

test_expect_success 'restore --staged --worktree from source' '
	(
	cd repo &&
	echo "modified both" >alpha.txt &&
	grit add alpha.txt &&
	grit restore --source HEAD --staged --worktree alpha.txt &&
	grep "v3 alpha" alpha.txt &&
	grit_status_clean >actual &&
	! grep "alpha.txt" actual
	)
'

test_expect_success 'restore -S -W from source' '
	(
	cd repo &&
	echo "changed again" >beta.txt &&
	grit add beta.txt &&
	grit restore -s HEAD -S -W beta.txt &&
	grep "original beta" beta.txt &&
	grit_status_clean >actual &&
	! grep "beta.txt" actual
	)
'

###########################################################################
# Section 6: Pathspec matching
###########################################################################

test_expect_success 'restore specific file leaves others dirty' '
	(
	cd repo &&
	echo "d1" >alpha.txt &&
	echo "d2" >beta.txt &&
	grit restore alpha.txt &&
	grit_status_clean >actual &&
	! grep "alpha.txt" actual &&
	grep "beta.txt" actual
	)
'

test_expect_success 'clean up' '
	(
	cd repo &&
	grit restore beta.txt
	)
'

test_expect_success 'restore with sub-directory file path' '
	(
	cd repo &&
	echo "sub dirty" >sub/file.txt &&
	grit restore sub/file.txt &&
	grep "original sub" sub/file.txt
	)
'

###########################################################################
# Section 7: --quiet
###########################################################################

test_expect_success 'restore --quiet suppresses output' '
	(
	cd repo &&
	echo "noisy" >alpha.txt &&
	grit restore --quiet alpha.txt 2>err &&
	test_must_be_empty err
	)
'

test_expect_success 'restore -q is same as --quiet' '
	(
	cd repo &&
	echo "noisy2" >beta.txt &&
	grit restore -q beta.txt 2>err &&
	test_must_be_empty err
	)
'

###########################################################################
# Section 8: Edge cases
###########################################################################

test_expect_success 'restore nonexistent file fails' '
	(
	cd repo &&
	test_must_fail grit restore nonexistent.txt 2>err
	)
'

test_expect_success 'restore --staged on clean file is no-op' '
	(
	cd repo &&
	grit restore --staged alpha.txt &&
	grit_status_clean >actual &&
	! grep "alpha.txt" actual
	)
'

test_expect_success 'restore --worktree on clean file is no-op' '
	(
	cd repo &&
	grit restore --worktree alpha.txt &&
	grep "v3 alpha" alpha.txt
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
	echo "content" >f.txt &&
	$REAL_GIT add f.txt &&
	test_tick &&
	$REAL_GIT commit -m "init" &&
	cd .. &&
	grit init grit-repo &&
	cd grit-repo &&
	echo "content" >f.txt &&
	grit add f.txt &&
	test_tick &&
	grit commit -m "init"
	)
'

test_expect_success 'restore --staged matches real git behavior' '
	echo "modified" >>git-repo/f.txt &&
	echo "modified" >>grit-repo/f.txt &&
	$REAL_GIT -C git-repo add f.txt &&
	grit -C grit-repo add f.txt &&
	$REAL_GIT -C git-repo restore --staged f.txt &&
	grit -C grit-repo restore --staged f.txt &&
	$REAL_GIT -C git-repo diff --name-only >expect &&
	grit -C grit-repo diff --name-only >actual &&
	test_cmp expect actual
'

test_expect_success 'restore worktree matches real git behavior' '
	$REAL_GIT -C git-repo restore f.txt &&
	grit -C grit-repo restore f.txt &&
	diff git-repo/f.txt grit-repo/f.txt
'

test_expect_success 'restore --staged after add in cross-check' '
	echo "extra" >git-repo/g.txt &&
	echo "extra" >grit-repo/g.txt &&
	$REAL_GIT -C git-repo add g.txt &&
	grit -C grit-repo add g.txt &&
	$REAL_GIT -C git-repo restore --staged g.txt &&
	grit -C grit-repo restore --staged g.txt &&
	$REAL_GIT -C git-repo ls-files >expect &&
	grit -C grit-repo ls-files >actual &&
	test_cmp expect actual
'

test_expect_success 'restore worktree from source in cross-check' '
	echo "mod2" >>git-repo/f.txt &&
	echo "mod2" >>grit-repo/f.txt &&
	$REAL_GIT -C git-repo add f.txt &&
	grit -C grit-repo add f.txt &&
	test_tick &&
	$REAL_GIT -C git-repo commit -m "mod2" &&
	grit -C grit-repo commit -m "mod2" &&
	prev=$($REAL_GIT -C git-repo rev-parse HEAD~1) &&
	$REAL_GIT -C git-repo restore --source $prev f.txt &&
	grit_prev=$(grit -C grit-repo rev-parse HEAD~1) &&
	grit -C grit-repo restore --source $grit_prev f.txt &&
	diff git-repo/f.txt grit-repo/f.txt
'

test_expect_success 'restore specific path matches real git' '
	$REAL_GIT -C git-repo restore f.txt &&
	grit -C grit-repo restore f.txt &&
	diff git-repo/f.txt grit-repo/f.txt
'

test_expect_success 'restore --staged multiple files in cross-check' '
	echo "aa" >>git-repo/f.txt &&
	echo "aa" >>grit-repo/f.txt &&
	$REAL_GIT -C git-repo add f.txt &&
	grit -C grit-repo add f.txt &&
	$REAL_GIT -C git-repo restore --staged f.txt &&
	grit -C grit-repo restore --staged f.txt &&
	$REAL_GIT -C git-repo diff --name-only >expect &&
	grit -C grit-repo diff --name-only >actual &&
	test_cmp expect actual
'

test_expect_success 'restore after rm restores deleted file' '
	(
	cd repo &&
	rm sub/file.txt &&
	grit restore sub/file.txt &&
	test -f sub/file.txt &&
	grep "original sub" sub/file.txt
	)
'

test_expect_success 'restore --source with tag' '
	(
	cd repo &&
	grit tag v-test &&
	echo "after tag" >alpha.txt &&
	grit add alpha.txt &&
	test_tick &&
	grit commit -m "after tag" &&
	tag_oid=$(grit rev-parse v-test) &&
	grit restore --source $tag_oid alpha.txt &&
	grep "v3 alpha" alpha.txt
	)
'

test_expect_success 'file content matches after restore' '
	cat git-repo/f.txt >expect &&
	cat grit-repo/f.txt >actual &&
	test_cmp expect actual
'

test_done
