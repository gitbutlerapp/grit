#!/bin/sh
# Test grit branch --show-current and related branch listing/creation/deletion.

test_description='grit branch --show-current'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with initial commit' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "init" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "initial"
	)
'

test_expect_success 'show-current on main/main branch' '
	(
	cd repo &&
	grit branch --show-current >actual &&
	grep -qE "^(main|main)$" actual
	)
'

test_expect_success 'create new branch and switch to it' '
	(
	cd repo &&
	grit branch feature &&
	grit checkout feature &&
	grit branch --show-current >actual &&
	echo "feature" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'switch back to main' '
	(
	cd repo &&
	grit checkout main &&
	grit branch --show-current >actual &&
	echo "main" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'create and checkout branch in one step with checkout -b' '
	(
	cd repo &&
	grit checkout -b newbranch &&
	grit branch --show-current >actual &&
	echo "newbranch" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'show-current after switching branches multiple times' '
	(
	cd repo &&
	grit checkout main &&
	grit checkout feature &&
	grit checkout newbranch &&
	grit branch --show-current >actual &&
	echo "newbranch" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'branch list shows all branches' '
	(
	cd repo &&
	grit branch >actual &&
	grep "main" actual &&
	grep "feature" actual &&
	grep "newbranch" actual
	)
'

test_expect_success 'branch list marks current branch with asterisk' '
	(
	cd repo &&
	grit checkout main &&
	grit branch >actual &&
	grep "^\* main" actual
	)
'

test_expect_success 'create branch with slash in name' '
	(
	cd repo &&
	grit checkout main &&
	grit branch work/sub-task &&
	grit checkout work/sub-task &&
	grit branch --show-current >actual &&
	echo "work/sub-task" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'create branch from specific commit' '
	(
	cd repo &&
	grit checkout main &&
	oid=$(grit rev-parse HEAD) &&
	grit branch from-commit "$oid" &&
	grit checkout from-commit &&
	grit branch --show-current >actual &&
	echo "from-commit" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'delete branch that is not checked out' '
	(
	cd repo &&
	grit checkout main &&
	grit branch -d from-commit
	)
'

test_expect_success 'delete branch with -D (force)' '
	(
	cd repo &&
	grit checkout -b delete-me &&
	echo "extra" >extra.txt &&
	grit add extra.txt &&
	test_tick &&
	grit commit -m "extra" &&
	grit checkout main &&
	grit branch -D delete-me
	)
'

test_expect_success 'cannot delete current branch' '
	(
	cd repo &&
	grit checkout main &&
	! grit branch -d main 2>/dev/null
	)
'

test_expect_success 'show-current on detached HEAD is empty' '
	(
	cd repo &&
	oid=$(grit rev-parse HEAD) &&
	grit checkout "$oid" 2>/dev/null &&
	grit branch --show-current >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'reattach to branch after detached HEAD' '
	(
	cd repo &&
	grit checkout main &&
	grit branch --show-current >actual &&
	echo "main" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'branch -v shows verbose info' '
	(
	cd repo &&
	grit branch -v >actual &&
	grep "main" actual &&
	grep -qE "[0-9a-f]+" actual
	)
'

test_expect_success 'branch --list is same as branch with no args' '
	(
	cd repo &&
	grit branch >default_out &&
	grit branch --list >list_out &&
	test_cmp default_out list_out
	)
'

test_expect_success 'branch -m renames a branch' '
	(
	cd repo &&
	grit branch rename-me &&
	grit branch -m rename-me renamed &&
	grit branch >actual &&
	grep "renamed" actual &&
	! grep "rename-me" actual
	)
'

test_expect_success 'show-current after rename of current branch' '
	(
	cd repo &&
	grit checkout renamed &&
	grit branch -m renamed current-renamed &&
	grit branch --show-current >actual &&
	echo "current-renamed" >expected &&
	test_cmp expected actual &&
	grit checkout main
	)
'

test_expect_success 'branch from another branch tip' '
	(
	cd repo &&
	grit checkout main &&
	grit branch copied-branch current-renamed &&
	grit branch >actual &&
	grep "copied-branch" actual &&
	grep "current-renamed" actual
	)
'

test_expect_success 'branch --contains shows branches containing HEAD' '
	(
	cd repo &&
	grit checkout main &&
	grit branch --contains HEAD >actual &&
	grep "main" actual
	)
'

test_expect_success 'branch --merged HEAD shows merged branches' '
	(
	cd repo &&
	grit checkout main &&
	grit branch --merged HEAD >actual &&
	test_line_count -gt 0 actual
	)
'

test_expect_success 'create many branches' '
	(
	cd repo &&
	for i in 1 2 3 4 5 6 7 8 9 10; do
		grit branch "multi-$i" || return 1
	done &&
	grit branch >actual &&
	grep "multi-1" actual &&
	grep "multi-10" actual
	)
'

test_expect_success 'branch count is at least 10' '
	(
	cd repo &&
	grit branch >actual &&
	count=$(wc -l <actual | tr -d " ") &&
	test "$count" -ge 10
	)
'

test_expect_success 'delete multiple branches sequentially' '
	(
	cd repo &&
	grit branch -d multi-1 &&
	grit branch -d multi-2 &&
	grit branch -d multi-3 &&
	grit branch >actual &&
	! grep "multi-1$" actual &&
	! grep "multi-2$" actual &&
	! grep "multi-3$" actual
	)
'

test_expect_success 'branch with --force overwrites existing' '
	(
	cd repo &&
	grit checkout main &&
	echo "more" >more.txt &&
	grit add more.txt &&
	test_tick &&
	grit commit -m "more" &&
	grit branch --force feature &&
	oid_main=$(grit rev-parse main) &&
	oid_feature=$(grit rev-parse feature) &&
	test "$oid_main" = "$oid_feature"
	)
'

test_expect_success 'show-current output has no trailing spaces' '
	(
	cd repo &&
	grit branch --show-current >actual &&
	sed "s/ *$//" actual >clean &&
	test_cmp actual clean
	)
'

test_expect_success 'show-current output is single line' '
	(
	cd repo &&
	grit branch --show-current >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'branch with hyphen in name' '
	(
	cd repo &&
	grit branch my-hyphenated-branch &&
	grit checkout my-hyphenated-branch &&
	grit branch --show-current >actual &&
	echo "my-hyphenated-branch" >expected &&
	test_cmp expected actual &&
	grit checkout main
	)
'

test_expect_success 'branch with dots in name' '
	(
	cd repo &&
	grit branch release.1.0 &&
	grit checkout release.1.0 &&
	grit branch --show-current >actual &&
	echo "release.1.0" >expected &&
	test_cmp expected actual &&
	grit checkout main
	)
'

test_expect_success 'branch -a lists all including remotes placeholder' '
	(
	cd repo &&
	grit branch -a >actual &&
	test_line_count -gt 0 actual
	)
'

test_expect_success 'cleanup: delete test branches' '
	(
	cd repo &&
	grit checkout main &&
	for b in feature newbranch work/sub-task current-renamed copied-branch \
		multi-4 multi-5 multi-6 multi-7 multi-8 multi-9 multi-10 \
		my-hyphenated-branch release.1.0 renamed; do
		grit branch -D "$b" 2>/dev/null || true
	done &&
	grit branch >actual &&
	grep "main" actual
	)
'

test_done
