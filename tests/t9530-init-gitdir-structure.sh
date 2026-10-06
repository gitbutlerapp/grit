#!/bin/sh
# Tests for grit init: directory structure, --bare, --initial-branch,
# --separate-git-dir, --template, reinit, quiet mode, HEAD contents.

test_description='grit init gitdir structure and options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Basic init
###########################################################################

test_expect_success 'init creates .git directory' '
	grit init basic-repo &&
	test_path_is_dir basic-repo/.git
'

test_expect_success 'init creates HEAD file' '
	test -f basic-repo/.git/HEAD
'

test_expect_success 'HEAD points to refs/heads/master by default' '
	grep "ref: refs/heads/master" basic-repo/.git/HEAD ||
	grep "ref: refs/heads/main" basic-repo/.git/HEAD
'

test_expect_success 'init creates objects directory' '
	test_path_is_dir basic-repo/.git/objects
'

test_expect_success 'init creates objects/info directory' '
	test_path_is_dir basic-repo/.git/objects/info
'

test_expect_success 'init creates objects/pack directory' '
	test_path_is_dir basic-repo/.git/objects/pack
'

test_expect_success 'init creates refs directory' '
	test_path_is_dir basic-repo/.git/refs
'

test_expect_success 'init creates refs/heads directory' '
	test_path_is_dir basic-repo/.git/refs/heads
'

test_expect_success 'init creates refs/tags directory' '
	test_path_is_dir basic-repo/.git/refs/tags
'

test_expect_success 'init creates config file' '
	test -f basic-repo/.git/config
'

test_expect_success 'config has repositoryformatversion' '
	grep "repositoryformatversion" basic-repo/.git/config
'

test_expect_success 'config has bare = false for non-bare' '
	grep "bare = false" basic-repo/.git/config
'

###########################################################################
# Section 2: Init in current directory
###########################################################################

test_expect_success 'init in current directory works' '
	mkdir curdir-repo &&
	cd curdir-repo &&
	grit init &&
	test_path_is_dir .git &&
	cd ..
'

###########################################################################
# Section 3: --bare
###########################################################################

test_expect_success 'init --bare creates bare repository' '
	grit init --bare bare-repo &&
	test_path_is_dir bare-repo
'

test_expect_success 'bare repo has HEAD at top level' '
	test -f bare-repo/HEAD
'

test_expect_success 'bare repo has objects at top level' '
	test_path_is_dir bare-repo/objects
'

test_expect_success 'bare repo has refs at top level' '
	test_path_is_dir bare-repo/refs
'

test_expect_success 'bare repo has no .git directory' '
	test_path_is_missing bare-repo/.git
'

test_expect_success 'bare repo config has bare = true' '
	grep "bare = true" bare-repo/config
'

###########################################################################
# Section 4: --initial-branch / -b
###########################################################################

test_expect_success 'init -b sets initial branch name' '
	grit init -b develop branch-repo &&
	grep "ref: refs/heads/develop" branch-repo/.git/HEAD
'

test_expect_success 'init --initial-branch sets branch name' '
	grit init --initial-branch feature branch-repo2 &&
	grep "ref: refs/heads/feature" branch-repo2/.git/HEAD
'

test_expect_success 'init -b with bare repo' '
	grit init --bare -b trunk bare-branch &&
	grep "ref: refs/heads/trunk" bare-branch/HEAD
'

###########################################################################
# Section 5: Reinitializing
###########################################################################

test_expect_success 'reinit existing repo succeeds' '
	grit init reinit-repo &&
	echo "data" >reinit-repo/file.txt &&
	grit init reinit-repo
'

test_expect_success 'reinit preserves existing files' '
	test -f reinit-repo/file.txt &&
	echo "data" >expect &&
	test_cmp expect reinit-repo/file.txt
'

test_expect_success 'reinit keeps .git directory intact' '
	test_path_is_dir reinit-repo/.git &&
	test -f reinit-repo/.git/HEAD &&
	test_path_is_dir reinit-repo/.git/objects
'

###########################################################################
# Section 6: --quiet / -q
###########################################################################

test_expect_success 'init -q suppresses output' '
	grit init -q quiet-repo >actual 2>&1 &&
	test_must_be_empty actual
'

test_expect_success 'init --quiet suppresses output' '
	grit init --quiet quiet-repo2 >actual 2>&1 &&
	test_must_be_empty actual
'

###########################################################################
# Section 7: Functional init tests
###########################################################################

test_expect_success 'init repo is functional: can add and commit' '
	grit init func-repo &&
	cd func-repo &&
	grit config set user.name "Test" &&
	grit config set user.email "test@test.com" &&
	echo "hello" >test.txt &&
	grit add test.txt &&
	grit commit -m "first commit" &&
	cd ..
'

test_expect_success 'init repo can create branches' '
	cd func-repo &&
	grit branch feature &&
	grit branch -l >actual &&
	grep "feature" actual &&
	cd ..
'

test_expect_success 'init repo can create tags' '
	cd func-repo &&
	grit tag v0.1 &&
	grit tag -l >actual &&
	grep "v0.1" actual &&
	cd ..
'

###########################################################################
# Section 8: Directory creation
###########################################################################

test_expect_success 'init creates nested parent directories' '
	grit init deep/nested/repo &&
	test_path_is_dir deep/nested/repo/.git
'

test_expect_success 'init in existing empty directory' '
	mkdir empty-dir &&
	grit init empty-dir &&
	test_path_is_dir empty-dir/.git
'

###########################################################################
# Section 9: Comparing with real git
###########################################################################

test_expect_success 'grit and git produce same HEAD content' '
	grit init grit-cmp &&
	$REAL_GIT init git-cmp &&
	cat grit-cmp/.git/HEAD >grit-head &&
	cat git-cmp/.git/HEAD >git-head &&
	test_cmp git-head grit-head
'

test_expect_success 'grit and git both create same directory structure' '
	test_path_is_dir grit-cmp/.git/objects &&
	test_path_is_dir grit-cmp/.git/refs/heads &&
	test_path_is_dir grit-cmp/.git/refs/tags &&
	test_path_is_dir git-cmp/.git/objects &&
	test_path_is_dir git-cmp/.git/refs/heads &&
	test_path_is_dir git-cmp/.git/refs/tags
'

test_expect_success 'grit bare and git bare have same structure' '
	grit init --bare grit-bare-cmp &&
	$REAL_GIT init --bare git-bare-cmp &&
	test -f grit-bare-cmp/HEAD &&
	test -f git-bare-cmp/HEAD &&
	test_path_is_dir grit-bare-cmp/objects &&
	test_path_is_dir git-bare-cmp/objects
'

test_expect_success 'grit -b and git -b produce same HEAD' '
	grit init -b myb grit-b-cmp &&
	$REAL_GIT init -b myb git-b-cmp &&
	cat grit-b-cmp/.git/HEAD >grit-bh &&
	cat git-b-cmp/.git/HEAD >git-bh &&
	test_cmp git-bh grit-bh
'

###########################################################################
# Section 10: Edge cases
###########################################################################

test_expect_success 'init with trailing slash' '
	grit init trailing-slash/ &&
	test_path_is_dir trailing-slash/.git
'

test_expect_success 'init twice is idempotent' '
	grit init idem-repo &&
	grit init idem-repo &&
	test_path_is_dir idem-repo/.git
'

test_done
