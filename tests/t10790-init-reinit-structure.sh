#!/bin/sh
# Test grit init and reinitialize behaviors: directory structure,
# bare repos, initial branch, templates, separate git dir, and reinit.

test_description='grit init and reinit repository structure'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Basic init
###########################################################################

test_expect_success 'init creates .git directory' '
	grit init basic-repo &&
	test_path_is_dir basic-repo/.git
'

test_expect_success 'init creates standard subdirectories' '
	test_path_is_dir basic-repo/.git/objects &&
	test_path_is_dir basic-repo/.git/refs &&
	test_path_is_dir basic-repo/.git/refs/heads &&
	test_path_is_dir basic-repo/.git/refs/tags
'

test_expect_success 'init creates HEAD file' '
	test_path_is_file basic-repo/.git/HEAD
'

test_expect_success 'HEAD points to refs/heads/main by default' '
	cat basic-repo/.git/HEAD >out &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect out
'

test_expect_success 'init creates config file' '
	test_path_is_file basic-repo/.git/config
'

test_expect_success 'init config has bare = false' '
	(
	cd basic-repo &&
	grit config get core.bare >out &&
	echo "false" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'init matches git directory structure' '
	git init git-repo &&
	test_path_is_dir git-repo/.git/objects &&
	test_path_is_dir git-repo/.git/refs/heads &&
	test_path_is_dir git-repo/.git/refs/tags &&
	test_path_is_file git-repo/.git/HEAD
'

###########################################################################
# Section 2: Init in current directory
###########################################################################

test_expect_success 'init with no args inits current directory' '
	mkdir cwd-repo && cd cwd-repo &&
	grit init &&
	test_path_is_dir .git &&
	test_path_is_file .git/HEAD
'

###########################################################################
# Section 3: Bare repository
###########################################################################

test_expect_success 'init --bare creates bare repository' '
	grit init --bare bare-repo &&
	test_path_is_dir bare-repo/objects &&
	test_path_is_dir bare-repo/refs
'

test_expect_success 'bare repo has HEAD at top level' '
	test_path_is_file bare-repo/HEAD
'

test_expect_success 'bare repo has no .git directory' '
	test_path_is_missing bare-repo/.git
'

test_expect_success 'bare repo config has bare = true' '
	(
	cd bare-repo &&
	grit config get core.bare >out &&
	echo "true" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'bare repo HEAD points to main' '
	cat bare-repo/HEAD >out &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect out
'

###########################################################################
# Section 4: Initial branch name
###########################################################################

test_expect_success 'init -b sets initial branch name' '
	grit init -b main branch-repo &&
	cat branch-repo/.git/HEAD >out &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect out
'

test_expect_success 'init --initial-branch sets branch name' '
	grit init --initial-branch develop dev-repo &&
	cat dev-repo/.git/HEAD >out &&
	echo "ref: refs/heads/develop" >expect &&
	test_cmp expect out
'

test_expect_success 'init -b with bare repo' '
	grit init --bare -b trunk bare-branch &&
	cat bare-branch/HEAD >out &&
	echo "ref: refs/heads/trunk" >expect &&
	test_cmp expect out
'

###########################################################################
# Section 5: Reinitialize
###########################################################################

test_expect_success 'reinit existing repo succeeds' '
	grit init reinit-repo &&
	grit init reinit-repo
'

test_expect_success 'reinit preserves HEAD' '
	cat reinit-repo/.git/HEAD >before &&
	grit init reinit-repo &&
	cat reinit-repo/.git/HEAD >after &&
	test_cmp before after
'

test_expect_success 'reinit preserves existing objects' '
	(
	cd reinit-repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "data" >file.txt &&
	grit add file.txt &&
	grit commit -m "first" &&
	grit rev-parse HEAD >hash_before &&
	cd .. &&
	grit init reinit-repo &&
	cd reinit-repo &&
	grit rev-parse HEAD >hash_after &&
	test_cmp hash_before hash_after
	)
'

test_expect_success 'reinit preserves committed data' '
	(
	cd reinit-repo &&
	grit log --oneline >out &&
	grep "first" out
	)
'

test_expect_success 'reinit preserves branches' '
	(
	cd reinit-repo &&
	grit branch feature &&
	cd .. &&
	grit init reinit-repo &&
	cd reinit-repo &&
	grit branch >out &&
	grep "feature" out
	)
'

###########################################################################
# Section 6: Quiet mode
###########################################################################

test_expect_success 'init --quiet suppresses output' '
	grit init --quiet quiet-repo >out 2>&1 &&
	test_must_be_empty out
'

test_expect_success 'init -q suppresses output' '
	grit init -q quiet-repo2 >out 2>&1 &&
	test_must_be_empty out
'

###########################################################################
# Section 7: Multiple inits in different locations
###########################################################################

test_expect_success 'init multiple repos in parallel' '
	grit init multi-a &&
	grit init multi-b &&
	grit init multi-c &&
	test_path_is_dir multi-a/.git &&
	test_path_is_dir multi-b/.git &&
	test_path_is_dir multi-c/.git
'

test_expect_success 'each repo has independent HEAD' '
	grit init -b alpha headA &&
	grit init -b beta headB &&
	cat headA/.git/HEAD >outA &&
	cat headB/.git/HEAD >outB &&
	echo "ref: refs/heads/alpha" >expectA &&
	echo "ref: refs/heads/beta" >expectB &&
	test_cmp expectA outA &&
	test_cmp expectB outB
'

test_expect_success 'init in nested directory path' '
	grit init deep/nested/repo &&
	test_path_is_dir deep/nested/repo/.git &&
	test_path_is_file deep/nested/repo/.git/HEAD
'

###########################################################################
# Section 8: Error cases
###########################################################################

test_expect_success 'init in a file path fails' '
	echo "not a dir" >afile &&
	test_must_fail grit init afile
'

test_expect_success 'init with custom branch name containing slash' '
	grit init -b "feature/init" slash-branch-repo &&
	cat slash-branch-repo/.git/HEAD >out &&
	echo "ref: refs/heads/feature/init" >expect &&
	test_cmp expect out
'

###########################################################################
# Section 9: Objects directory structure
###########################################################################

test_expect_success 'init creates objects/pack directory' '
	grit init objcheck &&
	test_path_is_dir objcheck/.git/objects/pack
'

test_expect_success 'init creates objects/info directory' '
	test_path_is_dir objcheck/.git/objects/info
'

test_expect_success 'bare init creates objects/pack directory' '
	grit init --bare bareobj &&
	test_path_is_dir bareobj/objects/pack
'

test_expect_success 'init creates description file or not (match git)' '
	(
	grit init desccheck &&
	cd desccheck &&
	grit rev-parse --git-dir >out &&
	test "$(cat out)" = ".git"
	)
'

test_expect_success 'bare repo has config file' '
	grit init --bare barecfg &&
	test_path_is_file barecfg/config
'

test_done
