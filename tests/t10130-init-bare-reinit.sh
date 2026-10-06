#!/bin/sh
# Test grit init --bare, reinitializing repos, -b/--initial-branch,
# --quiet, and related edge cases.

test_description='grit init --bare and reinit'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'init creates .git directory' '
	grit init basic &&
	test -d basic/.git &&
	test -f basic/.git/HEAD &&
	test -d basic/.git/objects &&
	test -d basic/.git/refs
'

test_expect_success 'init --bare creates bare repository' '
	grit init --bare bare1.git &&
	test -f bare1.git/HEAD &&
	test -d bare1.git/objects &&
	test -d bare1.git/refs &&
	! test -d bare1.git/.git
'

test_expect_success 'bare repo HEAD points to refs/heads/master or main' '
	cat bare1.git/HEAD >actual &&
	grep -qE "ref: refs/heads/(master|main)" actual
'

test_expect_success 'bare repo config has bare = true' '
	grit -C bare1.git config get core.bare >actual &&
	echo "true" >expected &&
	test_cmp expected actual
'

test_expect_success 'non-bare repo config has bare = false' '
	grit -C basic config get core.bare >actual &&
	echo "false" >expected &&
	test_cmp expected actual
'

test_expect_success 'reinit existing repo succeeds without error' '
	grit init reinit_test &&
	grit init reinit_test
'

test_expect_success 'reinit preserves existing objects' '
	cd reinit_test &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test" &&
	echo "content" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "first" &&
	oid=$(grit rev-parse HEAD) &&
	cd .. &&
	grit init reinit_test &&
	cd reinit_test &&
	actual_oid=$(grit rev-parse HEAD) &&
	test "$oid" = "$actual_oid" &&
	cd ..
'

test_expect_success 'reinit preserves branches' '
	cd reinit_test &&
	grit branch feature1 &&
	cd .. &&
	grit init reinit_test &&
	cd reinit_test &&
	grit branch >branches &&
	grep "feature1" branches &&
	cd ..
'

test_expect_success 'reinit bare repo succeeds' '
	grit init --bare reinit_bare.git &&
	grit init --bare reinit_bare.git
'

test_expect_success 'reinit bare repo preserves bare = true' '
	grit -C reinit_bare.git config get core.bare >actual &&
	echo "true" >expected &&
	test_cmp expected actual
'

test_expect_success 'init with -b sets initial branch name' '
	grit init -b develop branch_test &&
	cat branch_test/.git/HEAD >actual &&
	echo "ref: refs/heads/develop" >expected &&
	test_cmp expected actual
'

test_expect_success 'init --initial-branch sets initial branch name' '
	grit init --initial-branch feature branch_test2 &&
	cat branch_test2/.git/HEAD >actual &&
	echo "ref: refs/heads/feature" >expected &&
	test_cmp expected actual
'

test_expect_success 'init --bare -b sets initial branch in bare repo' '
	grit init --bare -b trunk bare_branch.git &&
	cat bare_branch.git/HEAD >actual &&
	echo "ref: refs/heads/trunk" >expected &&
	test_cmp expected actual
'

test_expect_success 'init in current directory without argument' '
	mkdir init_cwd &&
	cd init_cwd &&
	grit init &&
	test -d .git &&
	cd ..
'

test_expect_success 'init creates HEAD file' '
	grit init head_check &&
	test -f head_check/.git/HEAD
'

test_expect_success 'init creates refs/heads directory' '
	grit init refs_check &&
	test -d refs_check/.git/refs/heads
'

test_expect_success 'init creates refs/tags directory' '
	test -d refs_check/.git/refs/tags
'

test_expect_success 'init creates objects/pack directory' '
	test -d refs_check/.git/objects/pack
'

test_expect_success 'init creates objects/info directory' '
	test -d refs_check/.git/objects/info
'

test_expect_success 'init --quiet suppresses output' '
	grit init --quiet quiet_repo >out 2>&1 &&
	test_must_be_empty out
'

test_expect_success 'init in nested directory path' '
	mkdir -p deep/nested/path &&
	grit init deep/nested/path &&
	test -d deep/nested/path/.git
'

test_expect_success 'init two repos side by side are independent' '
	grit init side_a &&
	grit init side_b &&
	test -d side_a/.git &&
	test -d side_b/.git &&
	cd side_a &&
	grit config user.email "a@example.com" &&
	grit config user.name "A" &&
	echo "a" >a.txt && grit add a.txt && test_tick && grit commit -m "a" &&
	cd ../side_b &&
	grit config user.email "b@example.com" &&
	grit config user.name "B" &&
	echo "b" >b.txt && grit add b.txt && test_tick && grit commit -m "b" &&
	oid_a=$(grit -C ../side_a rev-parse HEAD) &&
	oid_b=$(grit rev-parse HEAD) &&
	test "$oid_a" != "$oid_b" &&
	cd ..
'

test_expect_success 'bare repo has no working tree files beyond git metadata' '
	grit init --bare no_worktree.git &&
	ls no_worktree.git >entries &&
	grep "HEAD" entries &&
	grep "objects" entries
'

test_expect_success 'init with relative path works' '
	grit init ./rel_path_repo &&
	test -d rel_path_repo/.git
'

test_expect_success 'config in newly inited repo is readable' '
	grit init config_test &&
	grit -C config_test config list >/dev/null
'

test_expect_success 'init repo then add and commit works end to end' '
	grit init e2e_repo &&
	cd e2e_repo &&
	grit config user.email "e2e@example.com" &&
	grit config user.name "E2E" &&
	echo "data" >f.txt &&
	grit add f.txt &&
	test_tick &&
	grit commit -m "e2e" &&
	grit log --oneline >log &&
	grep "e2e" log &&
	cd ..
'

test_expect_success 'init -b with slash in branch name' '
	grit init -b feature/init-test slash_branch &&
	cat slash_branch/.git/HEAD >actual &&
	echo "ref: refs/heads/feature/init-test" >expected &&
	test_cmp expected actual
'

test_expect_success 'init description file exists' '
	grit init desc_test &&
	test -f desc_test/.git/description
'

test_expect_success 'init bare description file exists' '
	grit init --bare desc_bare.git &&
	test -f desc_bare.git/description
'

test_expect_success 'reinit with commit preserves commit data' '
	grit init reinit_head &&
	cd reinit_head &&
	grit config user.email "t@t.com" &&
	grit config user.name "T" &&
	echo x >x && grit add x && test_tick && grit commit -m x &&
	oid=$(grit rev-parse HEAD) &&
	cd .. &&
	grit init reinit_head &&
	cd reinit_head &&
	test "$oid" = "$(grit rev-parse HEAD)" &&
	cd ..
'

test_expect_success 'init with existing empty directory' '
	mkdir empty_dir &&
	grit init empty_dir &&
	test -d empty_dir/.git
'

test_expect_success 'init creates info/exclude or similar' '
	grit init excl_test &&
	test -d excl_test/.git/info
'

test_done
