#!/bin/sh
# Test grit cherry-pick with single commits, ranges, -n, conflicts.

test_description='grit cherry-pick range'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with two branches' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo base >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "base" &&
	grit tag base &&
	grit switch -c feature &&
	echo feat1 >feat1.txt &&
	grit add feat1.txt &&
	test_tick &&
	grit commit -m "feature-1" &&
	grit tag f1 &&
	echo feat2 >feat2.txt &&
	grit add feat2.txt &&
	test_tick &&
	grit commit -m "feature-2" &&
	grit tag f2 &&
	echo feat3 >feat3.txt &&
	grit add feat3.txt &&
	test_tick &&
	grit commit -m "feature-3" &&
	grit tag f3 &&
	grit switch main
	)
'

test_expect_success 'cherry-pick single commit' '
	(
	cd repo &&
	grit cherry-pick f1 &&
	test_path_is_file feat1.txt &&
	cat feat1.txt | grep "feat1"
	)
'

test_expect_success 'cherry-pick commit appears in log' '
	(
	cd repo &&
	grit log --oneline >log &&
	grep "feature-1" log
	)
'

test_expect_success 'cherry-pick does not bring other feature commits' '
	(
	cd repo &&
	test_path_is_missing feat2.txt &&
	test_path_is_missing feat3.txt
	)
'

test_expect_success 'cherry-pick range applies multiple commits' '
	(
	cd repo &&
	grit cherry-pick f2..f3 &&
	test_path_is_file feat3.txt &&
	cat feat3.txt | grep "feat3"
	)
'

test_expect_success 'cherry-pick range appears in log' '
	(
	cd repo &&
	grit log --oneline >log &&
	grep "feature-3" log
	)
'

test_expect_success 'cherry-pick range is exclusive on left side' '
	(
	cd repo &&
	grit log --oneline >log &&
	! grep "feature-2" log || test_path_is_file feat2.txt
	)
'

test_expect_success 'cherry-pick -n does not auto-commit' '
	(
	cd repo &&
	grit cherry-pick -n f2 &&
	test_path_is_file feat2.txt &&
	grit diff --cached --name-only >cached &&
	grep "feat2.txt" cached
	)
'

test_expect_success 'cherry-pick -n stages the change' '
	(
	cd repo &&
	grit diff --cached >diff &&
	grep "feat2" diff
	)
'

test_expect_success 'cherry-pick -n can be committed manually' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "manual cherry-pick commit" &&
	grit log --oneline >log &&
	grep "manual cherry-pick commit" log
	)
'

test_expect_success 'setup second repo for conflict tests' '
	(
	rm -rf repo2 &&
	grit init repo2 &&
	cd repo2 &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo line1 >conflict.txt &&
	grit add conflict.txt &&
	test_tick &&
	grit commit -m "initial" &&
	grit tag initial &&
	grit switch -c branch-a &&
	echo "branch-a-change" >conflict.txt &&
	grit add conflict.txt &&
	test_tick &&
	grit commit -m "branch-a change" &&
	grit tag a1 &&
	grit switch main &&
	echo "main-change" >conflict.txt &&
	grit add conflict.txt &&
	test_tick &&
	grit commit -m "main change"
	)
'

test_expect_success 'cherry-pick conflicting commit fails' '
	(
	cd repo2 &&
	test_must_fail grit cherry-pick a1 2>err
	)
'

test_expect_success 'cherry-pick conflict leaves conflict markers' '
	(
	cd repo2 &&
	grep "<<<<<<<" conflict.txt || grep "=======" conflict.txt
	)
'

test_expect_success 'cherry-pick --abort cleans up conflict' '
	(
	cd repo2 &&
	grit cherry-pick --abort &&
	cat conflict.txt | grep "main-change"
	)
'

test_expect_success 'setup third repo for multiple cherry-picks' '
	(
	rm -rf repo3 &&
	grit init repo3 &&
	cd repo3 &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo base >base.txt &&
	grit add base.txt &&
	test_tick &&
	grit commit -m "base" &&
	grit tag base3 &&
	grit switch -c dev &&
	echo d1 >d1.txt &&
	grit add d1.txt &&
	test_tick &&
	grit commit -m "dev-1" &&
	grit tag d1 &&
	echo d2 >d2.txt &&
	grit add d2.txt &&
	test_tick &&
	grit commit -m "dev-2" &&
	grit tag d2 &&
	echo d3 >d3.txt &&
	grit add d3.txt &&
	test_tick &&
	grit commit -m "dev-3" &&
	grit tag d3 &&
	echo d4 >d4.txt &&
	grit add d4.txt &&
	test_tick &&
	grit commit -m "dev-4" &&
	grit tag d4 &&
	grit switch main
	)
'

test_expect_success 'cherry-pick range brings commits' '
	(
	cd repo3 &&
	grit cherry-pick base3..d2 &&
	test_path_is_file d1.txt &&
	test_path_is_file d2.txt &&
	test_path_is_missing d3.txt
	)
'

test_expect_success 'cherry-pick range commits appear in log' '
	(
	cd repo3 &&
	grit log --oneline >log &&
	grep "dev-1" log &&
	grep "dev-2" log
	)
'

test_expect_success 'cherry-pick additional single commits' '
	(
	cd repo3 &&
	grit cherry-pick d3 &&
	test_path_is_file d3.txt
	)
'

test_expect_success 'cherry-pick preserves commit message' '
	(
	cd repo3 &&
	grit log --oneline >log &&
	grep "dev-3" log
	)
'

test_expect_success 'cherry-pick remaining commit d4' '
	(
	cd repo3 &&
	grit cherry-pick d4 &&
	test_path_is_file d4.txt &&
	cat d4.txt | grep "d4"
	)
'

test_expect_success 'all dev files present after cherry-picks' '
	(
	cd repo3 &&
	test_path_is_file d1.txt &&
	test_path_is_file d2.txt &&
	test_path_is_file d3.txt &&
	test_path_is_file d4.txt
	)
'

test_expect_success 'setup repo4 for cherry-pick -n range' '
	(
	rm -rf repo4 &&
	grit init repo4 &&
	cd repo4 &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo start >start.txt &&
	grit add start.txt &&
	test_tick &&
	grit commit -m "start" &&
	grit tag start &&
	grit switch -c picks &&
	echo p1 >p1.txt &&
	grit add p1.txt &&
	test_tick &&
	grit commit -m "pick-1" &&
	grit tag p1 &&
	echo p2 >p2.txt &&
	grit add p2.txt &&
	test_tick &&
	grit commit -m "pick-2" &&
	grit tag p2 &&
	grit switch main
	)
'

test_expect_success 'cherry-pick -n single commit stages without committing' '
	(
	cd repo4 &&
	grit cherry-pick -n p1 &&
	grit diff --cached --name-only >cached &&
	grep "p1.txt" cached &&
	grit log --oneline >log &&
	! grep "pick-1" log
	)
'

test_expect_success 'cherry-pick -n second commit accumulates' '
	(
	cd repo4 &&
	grit cherry-pick -n p2 &&
	grit diff --cached --name-only >cached &&
	grep "p1.txt" cached &&
	grep "p2.txt" cached
	)
'

test_expect_success 'cherry-pick -n accumulated commit as squash' '
	(
	cd repo4 &&
	test_tick &&
	grit commit -m "squashed picks" &&
	grit log --oneline >log &&
	grep "squashed picks" log &&
	test_line_count = 2 log
	)
'

test_expect_success 'cherry-pick onto branch with different file' '
	(
	cd repo4 &&
	echo extra >extra.txt &&
	grit add extra.txt &&
	test_tick &&
	grit commit -m "add extra on main" &&
	grit switch picks &&
	echo p3 >p3.txt &&
	grit add p3.txt &&
	test_tick &&
	grit commit -m "pick-3" &&
	grit tag p3 &&
	grit switch main &&
	grit cherry-pick p3 &&
	test_path_is_file p3.txt &&
	test_path_is_file extra.txt
	)
'

test_expect_success 'cherry-pick from tag reference' '
	(
	cd repo4 &&
	grit switch -c from-tag start &&
	grit cherry-pick p1 &&
	test_path_is_file p1.txt &&
	grit log --oneline >log &&
	grep "pick-1" log
	)
'

test_expect_success 'cherry-pick does not modify source branch' '
	(
	cd repo4 &&
	grit switch picks &&
	grit log --oneline >log &&
	! grep "squashed" log
	)
'

test_expect_success 'cherry-pick from different branch preserves both histories' '
	(
	cd repo4 &&
	grit switch main &&
	grit log --oneline >main_log &&
	grit switch picks &&
	grit log --oneline >picks_log &&
	! test_cmp main_log picks_log >/dev/null 2>&1 &&
	grit switch main
	)
'

test_expect_success 'cherry-pick --no-commit is same as -n' '
	(
	cd repo4 &&
	grit switch -c nocommit-test start &&
	grit cherry-pick --no-commit p1 &&
	grit diff --cached --name-only >cached &&
	grep "p1.txt" cached &&
	grit log --oneline >log &&
	! grep "pick-1" log &&
	grit reset --hard HEAD
	)
'

test_expect_success 'cherry-pick -n then reset cancels the pick' '
	(
	cd repo4 &&
	grit switch -c cancel-test start &&
	grit cherry-pick -n p2 &&
	test_path_is_file p2.txt &&
	grit reset --hard HEAD &&
	test_path_is_missing p2.txt
	)
'

test_done
