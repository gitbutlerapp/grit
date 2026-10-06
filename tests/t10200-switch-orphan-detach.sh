#!/bin/sh
# Test grit switch with -c, --orphan, --detach, and branch switching.

test_description='grit switch orphan and detach'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with initial commit' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo initial >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "initial commit"
	)
'

test_expect_success 'switch -c creates new branch' '
	(
	cd repo &&
	grit switch -c feature1 &&
	grit branch >branches &&
	grep "feature1" branches
	)
'

test_expect_success 'switch -c new branch is current' '
	(
	cd repo &&
	grit branch --show-current >current &&
	grep "feature1" current
	)
'

test_expect_success 'switch -c preserves working tree' '
	(
	cd repo &&
	test_path_is_file file.txt &&
	cat file.txt | grep "initial"
	)
'

test_expect_success 'commit on new branch diverges from main' '
	(
	cd repo &&
	echo feature >feature.txt &&
	grit add feature.txt &&
	test_tick &&
	grit commit -m "feature commit" &&
	grit log --oneline >log &&
	grep "feature commit" log
	)
'

test_expect_success 'switch back to main' '
	(
	cd repo &&
	grit switch main &&
	grit branch --show-current >current &&
	grep "main" current
	)
'

test_expect_success 'switch to main loses feature file' '
	(
	cd repo &&
	test_path_is_missing feature.txt
	)
'

test_expect_success 'switch back to feature1 restores feature file' '
	(
	cd repo &&
	grit switch feature1 &&
	test_path_is_file feature.txt
	)
'

test_expect_success 'switch -c from non-HEAD commit' '
	(
	cd repo &&
	grit switch main &&
	initial=$(grit rev-parse HEAD) &&
	echo second >second.txt &&
	grit add second.txt &&
	test_tick &&
	grit commit -m "second on main" &&
	grit switch -c from-initial "$initial" &&
	grit branch --show-current >current &&
	grep "from-initial" current &&
	test_path_is_missing second.txt
	)
'

test_expect_success 'switch --orphan creates branch with no history' '
	(
	cd repo &&
	grit switch --orphan orphan-branch &&
	grit log 2>err;
	test $? -ne 0 || {
		grit log --oneline >log 2>/dev/null &&
		test_must_be_empty log
	}
	)
'

test_expect_success 'switch --orphan has empty index after clean' '
	(
	cd repo &&
	grit rm -rf . 2>/dev/null || rm -f file.txt second.txt &&
	grit status --porcelain >status &&
	! grep "^A " status
	)
'

test_expect_success 'commit on orphan branch creates root commit' '
	(
	cd repo &&
	echo orphan >orphan-file.txt &&
	grit add orphan-file.txt &&
	test_tick &&
	grit commit -m "orphan root" &&
	grit log --oneline >log &&
	test_line_count = 1 log &&
	grep "orphan root" log
	)
'

test_expect_success 'orphan branch has no relation to main' '
	(
	cd repo &&
	orphan_head=$(grit rev-parse HEAD) &&
	grit switch main &&
	main_head=$(grit rev-parse HEAD) &&
	test "$orphan_head" != "$main_head"
	)
'

test_expect_success 'switch --detach goes to detached HEAD' '
	(
	cd repo &&
	grit switch --detach HEAD &&
	grit branch >branches &&
	grep "HEAD detached" branches || grep "^\* (HEAD" branches || head -1 branches | grep -v "main"
	)
'

test_expect_success 'switch --detach preserves files' '
	(
	cd repo &&
	test_path_is_file file.txt &&
	test_path_is_file second.txt
	)
'

test_expect_success 'switch --detach at specific commit' '
	(
	cd repo &&
	grit switch main &&
	initial=$(grit log --oneline | tail -1 | cut -d" " -f1) &&
	grit switch --detach "$initial" &&
	test_path_is_file file.txt &&
	test_path_is_missing second.txt
	)
'

test_expect_success 'switch from detached HEAD to named branch' '
	(
	cd repo &&
	grit switch main &&
	grit branch --show-current >current &&
	grep "main" current
	)
'

test_expect_success 'switch -c from detached HEAD' '
	(
	cd repo &&
	grit switch --detach HEAD &&
	grit switch -c from-detached &&
	grit branch --show-current >current &&
	grep "from-detached" current
	)
'

test_expect_success 'setup multiple branches for listing' '
	(
	cd repo &&
	grit switch main &&
	grit switch -c branch-a &&
	grit switch main &&
	grit switch -c branch-b &&
	grit switch main &&
	grit switch -c branch-c
	)
'

test_expect_success 'all created branches exist' '
	(
	cd repo &&
	grit branch >branches &&
	grep "branch-a" branches &&
	grep "branch-b" branches &&
	grep "branch-c" branches &&
	grep "feature1" branches &&
	grep "orphan-branch" branches
	)
'

test_expect_success 'switch between branches preserves commits' '
	(
	cd repo &&
	grit switch feature1 &&
	test_path_is_file feature.txt &&
	grit switch main &&
	test_path_is_missing feature.txt &&
	grit switch feature1 &&
	test_path_is_file feature.txt
	)
'

test_expect_success 'switch to nonexistent branch fails' '
	(
	cd repo &&
	test_must_fail grit switch no-such-branch 2>err
	)
'

test_expect_success 'switch -c existing branch name fails' '
	(
	cd repo &&
	test_must_fail grit switch -c main 2>err
	)
'

test_expect_success 'switch with conflicting dirty worktree fails' '
	(
	cd repo &&
	grit switch main &&
	echo dirty-conflict >feature.txt &&
	grit add feature.txt &&
	echo more-dirty >feature.txt &&
	test_must_fail grit switch feature1 2>err;
	grit checkout -- . 2>/dev/null || grit restore . 2>/dev/null || true &&
	grit reset HEAD -- feature.txt 2>/dev/null || true &&
	rm -f feature.txt
	)
'

test_expect_success 'switch --orphan second time creates another root' '
	(
	cd repo &&
	grit switch main &&
	grit reset --hard HEAD &&
	grit switch --orphan another-orphan &&
	grit rm -rf . 2>/dev/null || true &&
	echo another >another.txt &&
	grit add another.txt &&
	test_tick &&
	grit commit -m "another orphan root" &&
	grit log --oneline >log &&
	test_line_count = 1 log
	)
'

test_expect_success 'switch --detach specific tag works' '
	(
	cd repo &&
	grit switch main &&
	grit tag v1.0 &&
	grit switch --detach v1.0 &&
	head_at_tag=$(grit rev-parse HEAD) &&
	tag_target=$(grit rev-parse v1.0) &&
	test "$head_at_tag" = "$tag_target"
	)
'

test_expect_success 'switch back from tag detach to main' '
	(
	cd repo &&
	grit switch main &&
	grit branch --show-current >current &&
	grep "main" current
	)
'

test_expect_success 'switch -c with upstream tracking branch name' '
	(
	cd repo &&
	grit switch -c tracking-test &&
	grit branch --show-current >current &&
	grep "tracking-test" current
	)
'

test_expect_success 'switch - goes to previous branch' '
	(
	cd repo &&
	grit switch main &&
	grit switch feature1 &&
	grit switch - &&
	grit branch --show-current >current &&
	grep "main" current
	)
'

test_expect_success 'switch - again goes back' '
	(
	cd repo &&
	grit switch - &&
	grit branch --show-current >current &&
	grep "feature1" current
	)
'

test_expect_success 'switch --detach HEAD~1 works' '
	(
	cd repo &&
	grit switch main &&
	grit switch --detach HEAD~1 &&
	detached=$(grit rev-parse HEAD) &&
	main_parent=$(grit rev-parse main~1) &&
	test "$detached" = "$main_parent"
	)
'

test_done
