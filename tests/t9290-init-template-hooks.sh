#!/bin/sh
# Tests for init --template, hook directory creation, bare repos,
# --separate-git-dir, reinit behavior, and directory structure.

test_description='init --template, hooks, bare, separate-git-dir'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- basic init structure ----------------------------------------------------

test_expect_success 'init creates .git directory' '
	grit init basic-repo &&
	test_path_is_dir basic-repo/.git
'

test_expect_success 'init creates objects directory' '
	test_path_is_dir basic-repo/.git/objects
'

test_expect_success 'init creates refs directory' '
	test_path_is_dir basic-repo/.git/refs
'

test_expect_success 'init creates refs/heads' '
	test_path_is_dir basic-repo/.git/refs/heads
'

test_expect_success 'init creates refs/tags' '
	test_path_is_dir basic-repo/.git/refs/tags
'

test_expect_success 'init creates HEAD file' '
	test_path_is_file basic-repo/.git/HEAD
'

test_expect_success 'init HEAD points to initial branch' '
	grit init -b main head-repo &&
	cat head-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect actual
'

# -- template ----------------------------------------------------------------

test_expect_success 'setup: create flat template directory' '
	mkdir -p my-template &&
	echo "custom-info" >my-template/custom-info
'

test_expect_success 'init --template copies flat files' '
	grit init --template my-template tmpl-repo &&
	test_path_is_file tmpl-repo/.git/custom-info &&
	cat tmpl-repo/.git/custom-info >actual &&
	echo "custom-info" >expect &&
	test_cmp expect actual
'

test_expect_success 'init --template still creates objects dir' '
	test_path_is_dir tmpl-repo/.git/objects
'

test_expect_success 'init --template still creates refs dir' '
	test_path_is_dir tmpl-repo/.git/refs
'

test_expect_success 'init --template with empty template dir works' '
	mkdir -p empty-template &&
	grit init --template empty-template empty-tmpl-repo &&
	test_path_is_dir empty-tmpl-repo/.git
'

# -- bare repos --------------------------------------------------------------

test_expect_success 'init --bare creates bare repo' '
	grit init --bare bare-repo &&
	test_path_is_dir bare-repo/objects &&
	test_path_is_dir bare-repo/refs &&
	test_path_is_file bare-repo/HEAD
'

test_expect_success 'init --bare has no working tree' '
	test_path_is_missing bare-repo/.git
'

test_expect_success 'init --bare config has bare = true' '
	grit -C bare-repo config get core.bare >actual &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'init --bare -b main sets correct HEAD' '
	grit init --bare -b main bare-main &&
	cat bare-main/HEAD >actual &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect actual
'

# -- multiple inits with different branch names ------------------------------

test_expect_success 'init -b dev creates dev branch' '
	grit init -b dev dev-repo &&
	cat dev-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/dev" >expect &&
	test_cmp expect actual
'

test_expect_success 'init -b feature/test creates nested branch ref' '
	grit init -b feature/test nested-branch-repo &&
	cat nested-branch-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/feature/test" >expect &&
	test_cmp expect actual
'

test_expect_success 'init --initial-branch works same as -b' '
	grit init --initial-branch trunk ib-repo &&
	cat ib-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/trunk" >expect &&
	test_cmp expect actual
'

# -- quiet flag --------------------------------------------------------------

test_expect_success 'init -q produces no output on stdout' '
	grit init -q quiet-repo >actual 2>&1 &&
	test_must_be_empty actual
'

# -- reinit ------------------------------------------------------------------

test_expect_success 'init on existing repo does not destroy data' '
	grit init reinit-repo &&
	cd reinit-repo &&
	echo "data" >file.txt &&
	grit add file.txt &&
	grit commit -m "initial" &&
	cd .. &&
	grit init reinit-repo &&
	test_path_is_file reinit-repo/file.txt
'

test_expect_success 'reinit preserves existing files' '
	test_path_is_file reinit-repo/file.txt &&
	cat reinit-repo/file.txt >actual &&
	echo "data" >expect &&
	test_cmp expect actual
'

# -- init in current directory -----------------------------------------------

test_expect_success 'init with no args creates repo in current dir' '
	mkdir cwd-init &&
	cd cwd-init &&
	grit init &&
	test_path_is_dir .git &&
	cd ..
'

# -- comparison with real git ------------------------------------------------

test_expect_success 'setup: comparison repos' '
	$REAL_GIT init -b main git-comp &&
	grit init -b main grit-comp
'

test_expect_success 'init directory structure matches real git' '
	test_path_is_dir git-comp/.git/objects &&
	test_path_is_dir grit-comp/.git/objects &&
	test_path_is_dir git-comp/.git/refs/heads &&
	test_path_is_dir grit-comp/.git/refs/heads &&
	test_path_is_dir git-comp/.git/refs/tags &&
	test_path_is_dir grit-comp/.git/refs/tags
'

test_expect_success 'init HEAD content matches real git' '
	cat git-comp/.git/HEAD >expect &&
	cat grit-comp/.git/HEAD >actual &&
	test_cmp expect actual
'

test_expect_success 'init --bare structure matches real git' '
	$REAL_GIT init --bare -b main git-bare-comp &&
	grit init --bare -b main grit-bare-comp &&
	cat git-bare-comp/HEAD >expect &&
	cat grit-bare-comp/HEAD >actual &&
	test_cmp expect actual
'

test_expect_success 'init --bare has objects and refs like real git' '
	test_path_is_dir git-bare-comp/objects &&
	test_path_is_dir grit-bare-comp/objects &&
	test_path_is_dir git-bare-comp/refs &&
	test_path_is_dir grit-bare-comp/refs
'

# -- template with real git comparison ---------------------------------------

test_expect_success 'init config core.bare matches real git for normal repo' '
	$REAL_GIT -C git-comp config --get core.bare >expect &&
	grit -C grit-comp config get core.bare >actual &&
	test_cmp expect actual
'

test_expect_success 'init creates objects/info dir' '
	grit init objinfo-repo &&
	test_path_is_dir objinfo-repo/.git/objects
'

test_expect_success 'init creates objects/pack dir' '
	test_path_is_dir objinfo-repo/.git/objects/pack
'

test_done
