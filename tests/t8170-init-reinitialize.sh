#!/bin/sh
# Tests for reinitializing existing repos, --initial-branch changes,
# and various init edge cases.

test_description='init reinitialize and --initial-branch'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Basic reinit ─────────────────────────────────────────────────────────────

test_expect_success 'init: create initial repo' '
	git init reinit-repo &&
	test_path_is_dir reinit-repo/.git
'

test_expect_success 'reinit: reinitializing existing repo succeeds' '
	git init reinit-repo
'

test_expect_success 'reinit: .git directory still exists' '
	test_path_is_dir reinit-repo/.git
'

test_expect_success 'reinit: HEAD still exists' '
	test_path_is_file reinit-repo/.git/HEAD
'

test_expect_success 'reinit: objects directory still exists' '
	test_path_is_dir reinit-repo/.git/objects
'

test_expect_success 'reinit: refs directory still exists' '
	test_path_is_dir reinit-repo/.git/refs
'

test_expect_success 'reinit: preserves committed objects' '
	(
	cd reinit-repo &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "test content" >file.txt &&
	git add file.txt &&
	git commit -m "initial commit" &&
	git rev-parse HEAD >hash-before &&
	git init &&
	git rev-parse HEAD >hash-after &&
	test_cmp hash-before hash-after
	)
'

test_expect_success 'reinit: preserves tags' '
	(
	cd reinit-repo &&
	git tag test-tag HEAD &&
	git init &&
	git rev-parse test-tag >out &&
	git rev-parse HEAD >expected &&
	test_cmp expected out
	)
'

test_expect_success 'reinit: preserves index entries' '
	(
	cd reinit-repo &&
	echo "extra" >extra.txt &&
	git add extra.txt &&
	git init &&
	git ls-files >out &&
	grep "extra.txt" out
	)
'

test_expect_success 'reinit: core config is recreated' '
	(
	cd reinit-repo &&
	git init &&
	git config core.bare >out &&
	echo "false" >expected &&
	test_cmp expected out
	)
'

# ── --initial-branch / -b ───────────────────────────────────────────────────

test_expect_success 'init -b sets initial branch name' '
	(
	git init -b main branch-main &&
	cd branch-main &&
	grep "refs/heads/main" .git/HEAD
	)
'

test_expect_success 'init --initial-branch sets initial branch name' '
	(
	git init --initial-branch=develop branch-develop &&
	cd branch-develop &&
	grep "refs/heads/develop" .git/HEAD
	)
'

test_expect_success 'init -b with custom name' '
	(
	git init -b trunk branch-trunk &&
	cd branch-trunk &&
	grep "refs/heads/trunk" .git/HEAD
	)
'

test_expect_success 'init -b: first commit goes on named branch' '
	(
	cd branch-main &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "content" >f.txt &&
	git add f.txt &&
	git commit -m "first" &&
	git branch >out &&
	grep "main" out
	)
'

test_expect_success 'init without -b defaults to master' '
	(
	git init default-branch &&
	cd default-branch &&
	grep "refs/heads/master" .git/HEAD
	)
'

test_expect_success 'init -b with hyphenated name' '
	(
	git init -b my-branch hyphen-branch &&
	cd hyphen-branch &&
	grep "refs/heads/my-branch" .git/HEAD
	)
'

test_expect_success 'init -b with slashed name' '
	(
	git init -b feature/init slash-branch &&
	cd slash-branch &&
	grep "refs/heads/feature/init" .git/HEAD
	)
'

test_expect_success 'init -b with numeric name' '
	(
	git init -b v2 num-branch &&
	cd num-branch &&
	grep "refs/heads/v2" .git/HEAD
	)
'

# ── reinit with --initial-branch ─────────────────────────────────────────────

test_expect_success 'reinit with -b on empty repo does not change branch' '
	(
	git init -b old reinit-branch &&
	cd reinit-branch &&
	grep "refs/heads/old" .git/HEAD &&
	git init -b new &&
	grep "refs/heads/old" .git/HEAD
	)
'

test_expect_success 'reinit with -b on repo with commits' '
	(
	git init -b alpha reinit-committed &&
	cd reinit-committed &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "data" >f.txt &&
	git add f.txt &&
	git commit -m "commit" &&
	git init -b beta &&
	cat .git/HEAD >head-content &&
	grep "refs/heads" head-content
	)
'

# ── --bare ───────────────────────────────────────────────────────────────────

test_expect_success 'init --bare creates bare repo' '
	git init --bare bare-repo &&
	test_path_is_file bare-repo/HEAD &&
	test_path_is_dir bare-repo/objects &&
	test_path_is_missing bare-repo/.git
'

test_expect_success 'reinit --bare on existing bare repo succeeds' '
	git init --bare bare-repo
'

test_expect_success 'init --bare -b sets branch in bare repo' '
	git init --bare -b main bare-main &&
	grep "refs/heads/main" bare-main/HEAD
'

test_expect_success 'bare repo config has core.bare=true' '
	(
	cd bare-repo &&
	git config core.bare >out &&
	echo "true" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'bare repo has no working tree marker' '
	test_path_is_missing bare-repo/.git
'

# ── init with directory argument ─────────────────────────────────────────────

test_expect_success 'init creates directory if it does not exist' '
	git init new-dir-repo &&
	test_path_is_dir new-dir-repo/.git
'

test_expect_success 'init into nested non-existent path creates parents' '
	git init deep/nested/repo &&
	test_path_is_dir deep/nested/repo/.git
'

test_expect_success 'init in current directory (no arg)' '
	(
	mkdir cwd-repo &&
	cd cwd-repo &&
	git init &&
	test_path_is_dir .git
	)
'

# ── quiet flag ───────────────────────────────────────────────────────────────

test_expect_success 'init -q suppresses output' '
	git init -q quiet-repo >out 2>&1 &&
	test_must_be_empty out
'

test_expect_success 'reinit -q suppresses output' '
	git init -q quiet-repo >out 2>&1 &&
	test_must_be_empty out
'

# ── reinit preserves core config values ──────────────────────────────────────

test_expect_success 'reinit writes core.bare=false' '
	(
	git init preserve-core &&
	cd preserve-core &&
	git init &&
	git config core.bare >out &&
	echo "false" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'reinit writes core.filemode' '
	(
	cd preserve-core &&
	git init &&
	git config core.filemode >out &&
	test -s out
	)
'

test_expect_success 'reinit writes core.repositoryformatversion' '
	(
	cd preserve-core &&
	git init &&
	git config core.repositoryformatversion >out &&
	echo "0" >expected &&
	test_cmp expected out
	)
'

# ── multiple reinits ────────────────────────────────────────────────────────

test_expect_success 'multiple reinits in a row succeed' '
	git init multi-reinit &&
	git init multi-reinit &&
	git init multi-reinit &&
	git init multi-reinit &&
	test_path_is_dir multi-reinit/.git
'

test_expect_success 'reinit does not break ability to create commits' '
	(
	cd multi-reinit &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "data" >f.txt &&
	git add f.txt &&
	git commit -m "after reinit" &&
	git rev-parse HEAD
	)
'

test_done
