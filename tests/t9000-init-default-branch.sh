#!/bin/sh
# Tests for init with --initial-branch / -b flag and default branch behavior.

test_description='init --initial-branch / -b default branch name'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- basic init with -b -----------------------------------------------------

test_expect_success 'init -b main creates repo with main branch' '
	grit init -b main repo-main &&
	cd repo-main &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'init -b trunk creates repo with trunk branch' '
	grit init -b trunk repo-trunk &&
	cd repo-trunk &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/trunk" >expect &&
	test_cmp expect actual
'

test_expect_success 'init -b develop creates repo with develop branch' '
	grit init -b develop repo-develop &&
	cd repo-develop &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/develop" >expect &&
	test_cmp expect actual
'

test_expect_success 'init --initial-branch main works same as -b' '
	grit init --initial-branch main repo-ib &&
	cd repo-ib &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
'

# -- init without -b uses default -------------------------------------------

test_expect_success 'init without -b creates a repo' '
	grit init repo-default &&
	test -d repo-default/.git
'

test_expect_success 'init without -b HEAD points to a branch' '
	cd repo-default &&
	grit symbolic-ref HEAD >actual &&
	test -s actual
'

# -- init with -b and first commit ------------------------------------------

test_expect_success 'init -b main, commit, branch name is main' '
	grit init -b main repo-commit &&
	cd repo-commit &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "hello" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "first" &&
	grit branch --show-current >actual &&
	echo "main" >expect &&
	test_cmp expect actual
'

test_expect_success 'init -b custom-branch, commit, branch name is correct' '
	grit init -b custom-branch repo-custom &&
	cd repo-custom &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "hello" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "first" &&
	grit branch --show-current >actual &&
	echo "custom-branch" >expect &&
	test_cmp expect actual
'

# -- init creates .git structure ---------------------------------------------

test_expect_success 'init creates .git directory' '
	grit init repo-structure &&
	test -d repo-structure/.git
'

test_expect_success 'init creates .git/objects' '
	test -d repo-structure/.git/objects
'

test_expect_success 'init creates .git/refs' '
	test -d repo-structure/.git/refs
'

test_expect_success 'init creates .git/HEAD' '
	test -f repo-structure/.git/HEAD
'

test_expect_success 'init creates .git/refs/heads' '
	test -d repo-structure/.git/refs/heads
'

test_expect_success 'init creates .git/refs/tags' '
	test -d repo-structure/.git/refs/tags
'

# -- bare init ---------------------------------------------------------------

test_expect_success 'init --bare creates bare repo' '
	grit init --bare repo-bare.git &&
	test -f repo-bare.git/HEAD &&
	test -d repo-bare.git/objects &&
	test -d repo-bare.git/refs
'

test_expect_success 'init --bare -b main sets HEAD to refs/heads/main' '
	grit init --bare -b main repo-bare-main.git &&
	grit -C repo-bare-main.git symbolic-ref HEAD >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
'

# -- init -q (quiet) --------------------------------------------------------

test_expect_success 'init -q produces no output on stdout' '
	grit init -q repo-quiet >actual 2>/dev/null &&
	test_must_be_empty actual
'

# -- reinit existing repo ---------------------------------------------------

test_expect_success 'init on existing repo reinitializes' '
	grit init repo-reinit &&
	grit init repo-reinit
'

test_expect_success 'reinit does not destroy .git directory' '
	test -d repo-reinit/.git &&
	test -d repo-reinit/.git/objects &&
	test -d repo-reinit/.git/refs
'

# -- init with directory argument -------------------------------------------

test_expect_success 'init with nested directory creates parents' '
	grit init nested/deep/repo &&
	test -d nested/deep/repo/.git
'

test_expect_success 'init with nested directory and -b works' '
	grit init -b main nested/deep/repo2 &&
	cd nested/deep/repo2 &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
'

# -- compare with real git ---------------------------------------------------

test_expect_success 'init -b main matches real git structure' '
	grit init -b main cmp-grit &&
	$REAL_GIT init -b main cmp-git &&
	test -d cmp-grit/.git/objects &&
	test -d cmp-git/.git/objects &&
	test -d cmp-grit/.git/refs &&
	test -d cmp-git/.git/refs
'

test_expect_success 'init -b main HEAD matches real git HEAD' '
	grit -C cmp-grit symbolic-ref HEAD >actual &&
	$REAL_GIT -C cmp-git symbolic-ref HEAD >expect &&
	test_cmp expect actual
'

test_expect_success 'init -b trunk HEAD matches real git HEAD' '
	grit init -b trunk cmp-grit-trunk &&
	$REAL_GIT init -b trunk cmp-git-trunk &&
	grit -C cmp-grit-trunk symbolic-ref HEAD >actual &&
	$REAL_GIT -C cmp-git-trunk symbolic-ref HEAD >expect &&
	test_cmp expect actual
'

test_done
