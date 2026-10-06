#!/bin/sh
# Tests for grit rm: --cached (index-only removal), worktree removal,
# -r (recursive), -f (force), -n (dry-run), -q (quiet), --ignore-unmatch,
# and cross-checks with real git.

test_description='grit rm --cached, worktree, recursive, force, dry-run'

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

test_expect_success 'setup: create repo with files and subdirectories' '
	(
	grit init repo &&
	cd repo &&
	echo "alpha" >alpha.txt &&
	echo "beta" >beta.txt &&
	mkdir -p sub/deep &&
	echo "sub1" >sub/one.txt &&
	echo "sub2" >sub/two.txt &&
	echo "deep" >sub/deep/leaf.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "initial commit"
	)
'

###########################################################################
# Section 2: --cached (remove from index only)
###########################################################################

test_expect_success 'rm --cached removes file from index' '
	(
	cd repo &&
	grit rm --cached alpha.txt &&
	grit ls-files >actual &&
	! grep "alpha.txt" actual
	)
'

test_expect_success 'rm --cached leaves file in working tree' '
	(
	cd repo &&
	test -f alpha.txt
	)
'

test_expect_success 'rm --cached shows in status as deleted from index' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	grep "^D  alpha.txt" actual
	)
'

test_expect_success 'restore alpha.txt to index' '
	(
	cd repo &&
	grit restore --staged alpha.txt &&
	grit ls-files >actual &&
	grep "alpha.txt" actual
	)
'

test_expect_success 'rm --cached with multiple files' '
	(
	cd repo &&
	grit rm --cached alpha.txt beta.txt &&
	grit ls-files >actual &&
	! grep "alpha.txt" actual &&
	! grep "beta.txt" actual
	)
'

test_expect_success 'rm --cached multiple files keeps them in worktree' '
	(
	cd repo &&
	test -f alpha.txt &&
	test -f beta.txt
	)
'

test_expect_success 'restore multiple files to index' '
	(
	cd repo &&
	grit restore --staged alpha.txt beta.txt &&
	grit ls-files >actual &&
	grep "alpha.txt" actual &&
	grep "beta.txt" actual
	)
'

###########################################################################
# Section 3: Worktree removal (no --cached)
###########################################################################

test_expect_success 'rm removes file from index and working tree' '
	(
	cd repo &&
	grit rm beta.txt &&
	! test -f beta.txt &&
	grit ls-files >actual &&
	! grep "beta.txt" actual
	)
'

test_expect_success 'rm shows as deleted in status' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	grep "^D  beta.txt" actual
	)
'

test_expect_success 'restore beta.txt to index and worktree' '
	(
	cd repo &&
	grit checkout HEAD -- beta.txt &&
	test -f beta.txt &&
	grit ls-files >actual &&
	grep "beta.txt" actual
	)
'

###########################################################################
# Section 4: Recursive removal (-r)
###########################################################################

test_expect_success 'rm -r removes directory contents from index' '
	(
	cd repo &&
	grit rm -r sub &&
	grit ls-files >actual &&
	! grep "sub/" actual
	)
'

test_expect_success 'rm -r removes directory contents from working tree' '
	(
	cd repo &&
	! test -f sub/one.txt &&
	! test -f sub/two.txt &&
	! test -f sub/deep/leaf.txt
	)
'

test_expect_success 'restore sub directory' '
	(
	cd repo &&
	grit checkout HEAD -- sub &&
	test -f sub/one.txt &&
	test -f sub/two.txt &&
	test -f sub/deep/leaf.txt
	)
'

test_expect_success 'rm without -r on directory fails' '
	(
	cd repo &&
	test_must_fail grit rm sub 2>err
	)
'

test_expect_success 'rm -r --cached on directory keeps working tree' '
	(
	cd repo &&
	grit rm -r --cached sub &&
	grit ls-files >actual &&
	! grep "sub/" actual &&
	test -f sub/one.txt &&
	test -f sub/two.txt
	)
'

test_expect_success 'restore sub to index' '
	(
	cd repo &&
	grit add sub &&
	grit ls-files >actual &&
	grep "sub/one.txt" actual
	)
'

###########################################################################
# Section 5: Force removal (-f)
###########################################################################

test_expect_success 'rm -f removes file with staged changes' '
	(
	cd repo &&
	echo "modified" >>alpha.txt &&
	grit add alpha.txt &&
	grit rm -f alpha.txt &&
	! test -f alpha.txt &&
	grit ls-files >actual &&
	! grep "alpha.txt" actual
	)
'

test_expect_success 'restore alpha after force remove' '
	(
	cd repo &&
	grit checkout HEAD -- alpha.txt &&
	test -f alpha.txt &&
	grit ls-files >actual &&
	grep "alpha.txt" actual
	)
'

###########################################################################
# Section 6: Dry-run (-n / --dry-run)
###########################################################################

test_expect_success 'rm --dry-run does not remove from index' '
	(
	cd repo &&
	grit rm --dry-run alpha.txt &&
	grit ls-files >actual &&
	grep "alpha.txt" actual
	)
'

test_expect_success 'rm --dry-run does not remove from working tree' '
	(
	cd repo &&
	test -f alpha.txt
	)
'

test_expect_success 'rm -n is same as --dry-run' '
	(
	cd repo &&
	grit rm -n beta.txt &&
	grit ls-files >actual &&
	grep "beta.txt" actual &&
	test -f beta.txt
	)
'

test_expect_success 'rm --dry-run with -r on directory' '
	(
	cd repo &&
	grit rm -n -r sub &&
	grit ls-files >actual &&
	grep "sub/" actual &&
	test -f sub/one.txt
	)
'

###########################################################################
# Section 7: Quiet mode (-q)
###########################################################################

test_expect_success 'rm -q suppresses output' '
	(
	cd repo &&
	grit rm -q alpha.txt >out 2>&1 &&
	test_must_be_empty out
	)
'

test_expect_success 'restore alpha after quiet remove' '
	(
	cd repo &&
	grit checkout HEAD -- alpha.txt &&
	test -f alpha.txt
	)
'

test_expect_success 'rm without -q produces output' '
	(
	cd repo &&
	grit rm alpha.txt >out 2>&1 &&
	test -s out
	)
'

test_expect_success 'restore alpha again' '
	(
	cd repo &&
	grit checkout HEAD -- alpha.txt &&
	test -f alpha.txt
	)
'

###########################################################################
# Section 8: --ignore-unmatch
###########################################################################

test_expect_success 'rm fails for nonexistent file' '
	(
	cd repo &&
	test_must_fail grit rm nonexistent.txt
	)
'

test_expect_success 'rm --ignore-unmatch succeeds for nonexistent file' '
	(
	cd repo &&
	grit rm --ignore-unmatch nonexistent.txt
	)
'

test_expect_success 'rm --ignore-unmatch with existing file still removes it' '
	(
	cd repo &&
	grit rm --ignore-unmatch alpha.txt &&
	grit ls-files >actual &&
	! grep "alpha.txt" actual
	)
'

test_expect_success 'restore alpha for edge case tests' '
	(
	cd repo &&
	grit checkout HEAD -- alpha.txt &&
	test -f alpha.txt
	)
'

###########################################################################
# Section 9: Edge cases
###########################################################################

test_expect_success 'rm --cached on already-deleted working-tree file' '
	(
	cd repo &&
	rm alpha.txt &&
	grit rm --cached alpha.txt &&
	grit ls-files >actual &&
	! grep "alpha.txt" actual
	)
'

test_expect_success 'restore alpha for more tests' '
	(
	cd repo &&
	grit checkout HEAD -- alpha.txt &&
	test -f alpha.txt
	)
'

test_expect_success 'rm --cached on file in subdirectory' '
	(
	cd repo &&
	grit rm --cached sub/one.txt &&
	grit ls-files >actual &&
	! grep "^sub/one.txt$" actual &&
	test -f sub/one.txt
	)
'

test_expect_success 'restore sub/one.txt to index' '
	(
	cd repo &&
	grit add sub/one.txt &&
	grit ls-files >actual &&
	grep "sub/one.txt" actual
	)
'

test_expect_success 'rm --cached on deeply nested file' '
	(
	cd repo &&
	grit rm --cached sub/deep/leaf.txt &&
	grit ls-files >actual &&
	! grep "sub/deep/leaf.txt" actual &&
	test -f sub/deep/leaf.txt
	)
'

test_expect_success 'restore deep file' '
	(
	cd repo &&
	grit add sub/deep/leaf.txt &&
	grit ls-files >actual &&
	grep "sub/deep/leaf.txt" actual
	)
'

###########################################################################
# Section 10: Cross-check with real git
###########################################################################

test_expect_success 'setup cross-check repos' '
	(
	$REAL_GIT init git-repo &&
	cd git-repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	mkdir sub &&
	echo "c" >sub/c.txt &&
	$REAL_GIT add . &&
	test_tick &&
	$REAL_GIT commit -m "init" &&
	cd .. &&
	grit init grit-repo &&
	cd grit-repo &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	mkdir sub &&
	echo "c" >sub/c.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "init"
	)
'

test_expect_success 'rm --cached: grit matches real git ls-files' '
	$REAL_GIT -C git-repo rm --cached a.txt &&
	grit -C grit-repo rm --cached a.txt &&
	$REAL_GIT -C git-repo ls-files >expect &&
	grit -C grit-repo ls-files >actual &&
	test_cmp expect actual
'

test_expect_success 'rm -r: grit matches real git ls-files' '
	$REAL_GIT -C git-repo rm -r sub &&
	grit -C grit-repo rm -r sub &&
	$REAL_GIT -C git-repo ls-files >expect &&
	grit -C grit-repo ls-files >actual &&
	test_cmp expect actual
'

test_done
