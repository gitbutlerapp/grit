#!/bin/sh
# Tests for grit init: various flags, bare repos, reinit, initial-branch.

test_description='grit init: flags, bare, reinit, initial-branch'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Basic init
###########################################################################

test_expect_success 'init creates .git directory' '
	git init plain-repo &&
	test_path_is_dir plain-repo/.git
'

test_expect_success 'init creates HEAD file' '
	test_path_is_file plain-repo/.git/HEAD
'

test_expect_success 'init creates refs directory' '
	test_path_is_dir plain-repo/.git/refs
'

test_expect_success 'init creates objects directory' '
	test_path_is_dir plain-repo/.git/objects
'

test_expect_success 'init creates config file' '
	test_path_is_file plain-repo/.git/config
'

test_expect_success 'init HEAD points to refs/heads branch' '
	head_content=$(cat plain-repo/.git/HEAD) &&
	case "$head_content" in
	"ref: refs/heads/"*) true ;;
	*) false ;;
	esac
'

test_expect_success 'init in existing empty directory' '
	mkdir existing-dir &&
	git init existing-dir &&
	test_path_is_dir existing-dir/.git
'

test_expect_success 'init creates refs/heads' '
	test_path_is_dir plain-repo/.git/refs/heads
'

test_expect_success 'init creates refs/tags' '
	test_path_is_dir plain-repo/.git/refs/tags
'

###########################################################################
# Section 2: init --bare
###########################################################################

test_expect_success 'init --bare creates bare repo' '
	git init --bare bare-repo &&
	test_path_is_file bare-repo/HEAD &&
	test_path_is_dir bare-repo/refs &&
	test_path_is_dir bare-repo/objects
'

test_expect_success 'init --bare has no .git directory' '
	test_path_is_missing bare-repo/.git
'

test_expect_success 'init --bare config has bare=true' '
	(
	cd bare-repo &&
	test "$(git config core.bare)" = "true"
	)
'

test_expect_success 'init --bare HEAD points to branch ref' '
	head_content=$(cat bare-repo/HEAD) &&
	case "$head_content" in
	"ref: refs/heads/"*) true ;;
	*) false ;;
	esac
'

test_expect_success 'non-bare init has bare=false' '
	(
	cd plain-repo &&
	test "$(git config core.bare)" = "false"
	)
'

###########################################################################
# Section 3: init --initial-branch / -b
###########################################################################

test_expect_success 'init --initial-branch=trunk sets branch' '
	git init --initial-branch=trunk branch-repo &&
	head_ref=$(cat branch-repo/.git/HEAD) &&
	test "$head_ref" = "ref: refs/heads/trunk"
'

test_expect_success 'init -b develop sets branch' '
	git init -b develop b-repo &&
	head_ref=$(cat b-repo/.git/HEAD) &&
	test "$head_ref" = "ref: refs/heads/develop"
'

test_expect_success 'init -b with --bare' '
	git init --bare -b release bare-branch &&
	head_ref=$(cat bare-branch/HEAD) &&
	test "$head_ref" = "ref: refs/heads/release"
'

test_expect_success 'init -b custom-name is functional' '
	(
	cd b-repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo hello >file &&
	git add file &&
	git commit -m "on develop" &&
	git log --oneline | grep "on develop"
	)
'

###########################################################################
# Section 4: init config
###########################################################################

test_expect_success 'init config has repositoryformatversion' '
	(
	cd plain-repo &&
	test "$(git config core.repositoryformatversion)" = "0"
	)
'

test_expect_success 'init config has filemode' '
	(
	cd plain-repo &&
	val=$(git config core.filemode) &&
	case "$val" in
	true|false) true ;;
	*) false ;;
	esac
	)
'

###########################################################################
# Section 5: Reinitialize
###########################################################################

test_expect_success 'reinit in existing repo is safe' '
	(
	git init reinit-repo &&
	cd reinit-repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo "data" >file.txt &&
	git add file.txt &&
	git commit -m "initial" &&
	cd .. &&
	git init reinit-repo &&
	cd reinit-repo &&
	git log --oneline | grep "initial"
	)
'

test_expect_success 'reinit does not destroy objects' '
	test_path_is_dir reinit-repo/.git/objects
'

test_expect_success 'reinit does not destroy refs' '
	test_path_is_dir reinit-repo/.git/refs
'

test_expect_success 'reinit preserves config file' '
	test_path_is_file reinit-repo/.git/config
'

test_expect_success 'reinit does not lose commits' '
	(
	cd reinit-repo &&
	git rev-parse HEAD >out &&
	test -s out
	)
'

###########################################################################
# Section 6: init with nested directories
###########################################################################

test_expect_success 'init creates parent directories if needed' '
	git init nested/deep/repo &&
	test_path_is_dir nested/deep/repo/.git
'

test_expect_success 'nested init repo is functional' '
	(
	cd nested/deep/repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo "deep" >deep.txt &&
	git add deep.txt &&
	git commit -m "deep commit" &&
	git log --oneline | grep "deep commit"
	)
'

###########################################################################
# Section 7: init functional tests
###########################################################################

test_expect_success 'freshly inited repo has clean status' '
	(
	git init fresh-repo &&
	cd fresh-repo &&
	git status >out 2>&1
	)
'

test_expect_success 'freshly inited repo can add and commit' '
	(
	cd fresh-repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo content >f.txt &&
	git add f.txt &&
	git commit -m "test" &&
	git log --oneline | grep "test"
	)
'

test_expect_success 'init repo can create branches' '
	(
	cd fresh-repo &&
	git checkout -b feature &&
	echo "feature work" >feat.txt &&
	git add feat.txt &&
	git commit -m "feature" &&
	git log --oneline | grep "feature"
	)
'

test_expect_success 'init repo can create tags' '
	(
	cd fresh-repo &&
	git tag v0.1 &&
	git tag -l >tags &&
	grep "v0.1" tags
	)
'

test_expect_success 'init repo config can be set and read' '
	(
	cd fresh-repo &&
	git config custom.key value &&
	test "$(git config custom.key)" = "value"
	)
'

test_expect_success 'init in current directory (no path arg)' '
	(
	mkdir curdir-init &&
	cd curdir-init &&
	git init &&
	test_path_is_dir .git
	)
'

test_expect_success 'init sets up description file or info dir' '
	git init desc-repo &&
	test_path_is_dir desc-repo/.git
'

test_done
