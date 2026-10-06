#!/bin/sh
# Tests for init --bare in various locations, config verification, HEAD.

test_description='init --bare extra scenarios'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- basic bare init ----------------------------------------------------------

test_expect_success 'bare init creates objects directory' '
	git init --bare bare1.git &&
	test_path_is_dir bare1.git/objects
'

test_expect_success 'bare init creates refs directory' '
	test_path_is_dir bare1.git/refs
'

test_expect_success 'bare init creates refs/heads' '
	test_path_is_dir bare1.git/refs/heads
'

test_expect_success 'bare init creates refs/tags' '
	test_path_is_dir bare1.git/refs/tags
'

test_expect_success 'bare init creates HEAD file' '
	test_path_is_file bare1.git/HEAD
'

test_expect_success 'bare init HEAD points to refs/heads/master by default' '
	printf "ref: refs/heads/master" >expected &&
	tr -d "\n" <bare1.git/HEAD >actual &&
	test_cmp expected actual
'

test_expect_success 'bare init creates config file' '
	test_path_is_file bare1.git/config
'

test_expect_success 'bare init config has bare = true' '
	grep -q "bare = true" bare1.git/config
'

# -- bare init with -b --------------------------------------------------------

test_expect_success 'bare init -b main sets initial branch to main' '
	git init --bare -b main bare-main.git &&
	printf "ref: refs/heads/main" >expected &&
	tr -d "\n" <bare-main.git/HEAD >actual &&
	test_cmp expected actual
'

test_expect_success 'bare init -b develop sets initial branch to develop' '
	git init --bare -b develop bare-dev.git &&
	printf "ref: refs/heads/develop" >expected &&
	tr -d "\n" <bare-dev.git/HEAD >actual &&
	test_cmp expected actual
'

test_expect_success 'bare init -b with slash in branch name' '
	git init --bare -b feature/test bare-slash.git &&
	printf "ref: refs/heads/feature/test" >expected &&
	tr -d "\n" <bare-slash.git/HEAD >actual &&
	test_cmp expected actual
'

# -- bare init in nested directories ------------------------------------------

test_expect_success 'bare init in deeply nested path creates all parents' '
	git init --bare deep/nested/path/repo.git &&
	test_path_is_dir deep/nested/path/repo.git/objects &&
	test_path_is_file deep/nested/path/repo.git/HEAD
'

test_expect_success 'bare init in relative path' '
	mkdir -p subdir &&
	git init --bare subdir/rel.git &&
	test_path_is_dir subdir/rel.git/objects
'

# -- bare init does not have working tree artifacts ----------------------------

test_expect_success 'bare repo has no .git subdirectory' '
	git init --bare no-dotgit.git &&
	test_path_is_missing no-dotgit.git/.git
'

test_expect_success 'bare repo objects dir is at top level' '
	test_path_is_dir no-dotgit.git/objects &&
	test_path_is_dir no-dotgit.git/refs
'

# -- reinit bare ---------------------------------------------------------------

test_expect_success 'reinit bare repo is idempotent' '
	git init --bare reinit-bare.git &&
	git init --bare reinit-bare.git &&
	test_path_is_file reinit-bare.git/HEAD &&
	test_path_is_dir reinit-bare.git/objects
'

test_expect_success 'reinit bare repo does not destroy existing refs' '
	git init --bare reinit-refs.git &&
	echo "dummy" >reinit-refs.git/refs/heads/testref &&
	git init --bare reinit-refs.git &&
	test_path_is_file reinit-refs.git/refs/heads/testref
'

# -- non-bare init config -----------------------------------------------------

test_expect_success 'non-bare init does not set bare = true in config' '
	git init nonbare &&
	! grep "bare = true" nonbare/.git/config
'

test_expect_success 'non-bare init HEAD points to master by default' '
	printf "ref: refs/heads/master" >expected &&
	tr -d "\n" <nonbare/.git/HEAD >actual &&
	test_cmp expected actual
'

test_expect_success 'non-bare init -b sets branch' '
	git init -b trunk nonbare-trunk &&
	printf "ref: refs/heads/trunk" >expected &&
	tr -d "\n" <nonbare-trunk/.git/HEAD >actual &&
	test_cmp expected actual
'

# -- quiet mode ----------------------------------------------------------------

test_expect_success 'bare init --quiet produces no stdout' '
	git init --bare --quiet quiet-bare.git >out 2>&1 &&
	test_must_be_empty out
'

test_expect_success 'non-bare init --quiet produces no stdout' '
	git init --quiet quiet-nonbare >out 2>&1 &&
	test_must_be_empty out
'

# -- template ------------------------------------------------------------------

test_expect_success 'bare init with --template copies template files' '
	mkdir -p tmpl-bare &&
	echo "hook content" >tmpl-bare/pre-commit &&
	git init --bare --template=tmpl-bare tmpl-test.git &&
	test_path_is_file tmpl-test.git/pre-commit &&
	echo "hook content" >expected &&
	test_cmp expected tmpl-test.git/pre-commit
'

test_expect_success 'bare init with empty template dir still creates skeleton' '
	mkdir -p tmpl-empty &&
	git init --bare --template=tmpl-empty empty-tmpl.git &&
	test_path_is_dir empty-tmpl.git/objects &&
	test_path_is_dir empty-tmpl.git/refs &&
	test_path_is_file empty-tmpl.git/HEAD
'

# -- separate-git-dir ----------------------------------------------------------

test_expect_success 'bare init with .git suffix convention' '
	git init --bare convention.git &&
	test_path_is_dir convention.git/objects &&
	test_path_is_file convention.git/HEAD &&
	test_path_is_file convention.git/config
'

# -- multiple inits with different branches ------------------------------------

test_expect_success 'init creates distinct repos with different branches' '
	git init -b alpha repo-alpha &&
	git init -b beta repo-beta &&
	printf "ref: refs/heads/alpha" >expected_a &&
	printf "ref: refs/heads/beta" >expected_b &&
	tr -d "\n" <repo-alpha/.git/HEAD >actual_a &&
	tr -d "\n" <repo-beta/.git/HEAD >actual_b &&
	test_cmp expected_a actual_a &&
	test_cmp expected_b actual_b
'

# -- init in current directory -------------------------------------------------

test_expect_success 'init with no directory argument uses cwd' '
	mkdir init-cwd &&
	cd init-cwd &&
	git init &&
	test_path_is_dir .git &&
	test_path_is_file .git/HEAD &&
	cd ..
'

test_expect_success 'bare init with no directory argument uses cwd' '
	mkdir bare-cwd && cd bare-cwd &&
	git init --bare &&
	test_path_is_file HEAD &&
	test_path_is_dir objects &&
	cd ..
'

# -- object format -------------------------------------------------------------

test_expect_success 'init --object-format=sha1 succeeds' '
	git init --object-format=sha1 sha1-repo &&
	test_path_is_dir sha1-repo/.git
'

test_done
