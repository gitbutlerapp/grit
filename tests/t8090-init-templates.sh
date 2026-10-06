#!/bin/sh
# Tests for init with --template, --bare, -b, and directory argument.

test_description='init template, bare, and branch options'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Basic init ───────────────────────────────────────────────────────────

test_expect_success 'init: creates .git directory' '
	git init basic-repo &&
	test_path_is_dir basic-repo/.git
'

test_expect_success 'init: creates HEAD pointing to master' '
	(
	cd basic-repo &&
	test_path_is_file .git/HEAD &&
	grep "refs/heads/master" .git/HEAD
	)
'

test_expect_success 'init: creates refs/heads and refs/tags' '
	(
	cd basic-repo &&
	test_path_is_dir .git/refs/heads &&
	test_path_is_dir .git/refs/tags
	)
'

test_expect_success 'init: creates objects directory' '
	(
	cd basic-repo &&
	test_path_is_dir .git/objects
	)
'

test_expect_success 'init: creates config file' '
	(
	cd basic-repo &&
	test_path_is_file .git/config
	)
'

test_expect_success 'init: config has bare = false' '
	(
	cd basic-repo &&
	git config --bool core.bare >out &&
	echo "false" >expected &&
	test_cmp expected out
	)
'

# ── --bare ───────────────────────────────────────────────────────────────

test_expect_success 'init --bare: creates bare repository' '
	git init --bare bare-repo.git &&
	test_path_is_dir bare-repo.git
'

test_expect_success 'init --bare: HEAD exists at top level' '
	test_path_is_file bare-repo.git/HEAD
'

test_expect_success 'init --bare: no .git subdirectory' '
	test_path_is_missing bare-repo.git/.git
'

test_expect_success 'init --bare: refs at top level' '
	test_path_is_dir bare-repo.git/refs/heads &&
	test_path_is_dir bare-repo.git/refs/tags
'

test_expect_success 'init --bare: objects at top level' '
	test_path_is_dir bare-repo.git/objects
'

test_expect_success 'init --bare: config has bare = true' '
	(
	cd bare-repo.git &&
	git config --bool core.bare >out &&
	echo "true" >expected &&
	test_cmp expected out
	)
'

# ── -b / --initial-branch ───────────────────────────────────────────────

test_expect_success 'init -b: sets custom initial branch name' '
	(
	git init -b main custom-branch-repo &&
	cd custom-branch-repo &&
	grep "refs/heads/main" .git/HEAD
	)
'

test_expect_success 'init --initial-branch: sets custom branch' '
	(
	git init --initial-branch=develop dev-repo &&
	cd dev-repo &&
	grep "refs/heads/develop" .git/HEAD
	)
'

test_expect_success 'init -b: bare repo also respects -b' '
	git init --bare -b trunk bare-trunk.git &&
	grep "refs/heads/trunk" bare-trunk.git/HEAD
'

# ── Reinitialize ─────────────────────────────────────────────────────────

test_expect_success 'init: reinitialize existing repo does not fail' '
	git init reinit-repo &&
	git init reinit-repo
'

test_expect_success 'init: reinitialize keeps .git directory' '
	test_path_is_dir reinit-repo/.git &&
	test_path_is_file reinit-repo/.git/HEAD
'

test_expect_success 'init: reinitialize HEAD still valid' '
	(
	cd reinit-repo &&
	cat .git/HEAD >out &&
	grep "refs/heads" out
	)
'

# ── --quiet ──────────────────────────────────────────────────────────────

test_expect_success 'init --quiet: no output on success' '
	git init --quiet quiet-repo >out 2>&1 &&
	test_must_be_empty out
'

# ── Directory argument ───────────────────────────────────────────────────

test_expect_success 'init: creates directory if it does not exist' '
	git init new-dir-repo &&
	test_path_is_dir new-dir-repo/.git
'

test_expect_success 'init: works in existing empty directory' '
	mkdir empty-dir &&
	git init empty-dir &&
	test_path_is_dir empty-dir/.git
'

test_expect_success 'init: works in current directory with no arg' '
	(
	mkdir cwd-repo && cd cwd-repo &&
	git init &&
	test_path_is_dir .git
	)
'

# ── Object format ────────────────────────────────────────────────────────

test_expect_success 'init --object-format=sha1 works' '
	git init --object-format=sha1 sha1-repo &&
	test_path_is_dir sha1-repo/.git
'

# ── Combination flags ────────────────────────────────────────────────────

test_expect_success 'init --bare -b: combine bare with custom branch' '
	(
	git init --bare -b release bare-release.git &&
	test_path_is_file bare-release.git/HEAD &&
	grep "refs/heads/release" bare-release.git/HEAD &&
	cd bare-release.git &&
	git config --bool core.bare >out &&
	echo "true" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'init --bare --quiet: bare and quiet combined' '
	git init --bare --quiet bare-quiet.git >out 2>&1 &&
	test_must_be_empty out &&
	test_path_is_file bare-quiet.git/HEAD
'

test_expect_success 'init: description file exists' '
	(
	cd basic-repo &&
	test_path_is_file .git/description
	)
'

test_expect_success 'init: hooks directory created' '
	(
	cd basic-repo &&
	test_path_is_dir .git/hooks
	)
'

test_expect_success 'init: info directory created' '
	(
	cd basic-repo &&
	test_path_is_dir .git/info
	)
'

test_done
