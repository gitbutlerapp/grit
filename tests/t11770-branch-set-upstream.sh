#!/bin/sh
#
# Tests for git branch operations: create, delete, rename, copy, list, tracking
#

test_description='branch creation, deletion, renaming, listing, and tracking'
. ./test-lib.sh

test_expect_success 'setup: create repo with commits' '
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "test@test.com" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "initial" >file &&
	git add file &&
	git commit -m "initial" &&
	echo "second" >>file &&
	git add file &&
	git commit -m "second" &&
	cd ..
'

R="$TRASH_DIRECTORY/repo"

test_expect_success 'branch lists current branch' '
	git -C "$R" branch >actual &&
	grep "main" actual
'

test_expect_success 'branch shows current branch with asterisk' '
	git -C "$R" branch >actual &&
	grep "^\\* main" actual
'

test_expect_success 'create feature branch' '
	git -C "$R" branch feature &&
	git -C "$R" branch >actual &&
	grep "feature" actual
'

test_expect_success 'create develop branch' '
	git -C "$R" branch develop &&
	git -C "$R" branch >actual &&
	grep "develop" actual
'

test_expect_success 'branch -a lists all branches' '
	git -C "$R" update-ref refs/remotes/origin/main HEAD &&
	git -C "$R" branch -a >actual &&
	grep "main" actual &&
	grep "feature" actual &&
	grep "origin/main" actual
'

test_expect_success 'branch -r lists remote branches' '
	git -C "$R" branch -r >actual &&
	grep "origin/main" actual
'

test_expect_success 'branch -v shows commit info' '
	git -C "$R" branch -v >actual &&
	grep "main" actual &&
	grep "second" actual
'

test_expect_success 'branch -vv shows verbose info' '
	git -C "$R" branch -vv >actual &&
	grep "main" actual
'

test_expect_success 'branch -d deletes merged branch' '
	git -C "$R" branch -d feature &&
	git -C "$R" branch >actual &&
	! grep "feature" actual
'

test_expect_success 'deleted branch ref is gone' '
	test_must_fail git -C "$R" rev-parse --verify refs/heads/feature
'

test_expect_success 'branch -D force deletes branch' '
	cd "$R" &&
	git checkout -b to-force-delete &&
	echo "diverge" >diverge-file &&
	git add diverge-file &&
	git commit -m "diverge" &&
	git checkout main &&
	git branch -D to-force-delete &&
	git branch >actual &&
	! grep "to-force-delete" actual &&
	cd "$TRASH_DIRECTORY"
'

test_expect_success 'branch -d on unmerged branch fails' '
	cd "$R" &&
	git checkout -b unmerged-br &&
	echo "unmerged" >unmerged-file &&
	git add unmerged-file &&
	git commit -m "unmerged" &&
	git checkout main &&
	test_must_fail git branch -d unmerged-br 2>err &&
	git branch -D unmerged-br &&
	cd "$TRASH_DIRECTORY"
'

test_expect_success 'branch -m renames branch' '
	git -C "$R" branch rename-me &&
	git -C "$R" branch -m rename-me renamed &&
	git -C "$R" branch >actual &&
	grep "renamed" actual &&
	! grep "rename-me" actual
'

test_expect_success 'renamed branch points to same commit' '
	git -C "$R" rev-parse renamed >actual &&
	git -C "$R" rev-parse main >expect &&
	test_cmp expect actual
'

test_expect_success 'branch -M force renames' '
	git -C "$R" branch force-rename &&
	git -C "$R" branch -M force-rename force-renamed &&
	git -C "$R" branch >actual &&
	grep "force-renamed" actual &&
	! grep "force-rename " actual
'

test_expect_success 'branch -c copies branch' '
	git -C "$R" branch -c renamed copied &&
	git -C "$R" rev-parse copied >actual &&
	git -C "$R" rev-parse renamed >expect &&
	test_cmp expect actual
'

test_expect_success 'both original and copy exist after -c' '
	git -C "$R" branch >actual &&
	grep "renamed" actual &&
	grep "copied" actual
'

test_expect_success 'branch --contains shows branches containing HEAD' '
	git -C "$R" branch --contains HEAD >actual &&
	grep "main" actual
'

test_expect_success 'branch --contains with older commit shows all' '
	FIRST=$(git -C "$R" rev-parse HEAD~1) &&
	git -C "$R" branch --contains "$FIRST" >actual &&
	grep "main" actual
'

test_expect_success 'branch --merged HEAD shows merged branches' '
	git -C "$R" branch --merged HEAD >actual &&
	grep "main" actual
'

test_expect_success 'branch --no-merged HEAD shows unmerged branches' '
	cd "$R" &&
	git checkout -b diverged &&
	echo "diverge-content" >diverge.txt &&
	git add diverge.txt &&
	git commit -m "diverge" &&
	git checkout main &&
	git branch --no-merged HEAD >actual &&
	grep "diverged" actual &&
	git branch -D diverged &&
	cd "$TRASH_DIRECTORY"
'

test_expect_success 'branch --show-current shows current branch' '
	git -C "$R" branch --show-current >actual &&
	echo "main" >expect &&
	test_cmp expect actual
'

test_expect_success 'branch --show-current after checkout' '
	cd "$R" &&
	git checkout develop &&
	git branch --show-current >actual &&
	echo "develop" >expect &&
	test_cmp expect actual &&
	git checkout main &&
	cd "$TRASH_DIRECTORY"
'

test_expect_success 'branch from specific commit' '
	FIRST=$(git -C "$R" rev-parse HEAD~1) &&
	git -C "$R" branch from-first "$FIRST" &&
	git -C "$R" rev-parse from-first >actual &&
	echo "$FIRST" >expect &&
	test_cmp expect actual
'

test_expect_success 'branch from tag' '
	git -C "$R" tag v1.0 HEAD &&
	git -C "$R" branch from-tag v1.0 &&
	git -C "$R" rev-parse from-tag >actual &&
	git -C "$R" rev-parse v1.0 >expect &&
	test_cmp expect actual
'

test_expect_success 'branch -f forces branch to new commit' '
	FIRST=$(git -C "$R" rev-parse HEAD~1) &&
	git -C "$R" branch -f from-tag "$FIRST" &&
	git -C "$R" rev-parse from-tag >actual &&
	echo "$FIRST" >expect &&
	test_cmp expect actual
'

test_expect_success 'branch -f on current branch fails' '
	test_must_fail git -C "$R" branch -f main HEAD~1 2>err
'

test_expect_success 'create many branches' '
	for i in 1 2 3 4 5 6 7 8; do
		git -C "$R" branch "multi-$i" || return 1
	done &&
	git -C "$R" branch >actual &&
	for i in 1 2 3 4 5 6 7 8; do
		grep "multi-$i" actual || return 1
	done
'

test_expect_success 'branch -d multiple branches one at a time' '
	git -C "$R" branch -d multi-1 &&
	git -C "$R" branch -d multi-2 &&
	git -C "$R" branch -d multi-3 &&
	git -C "$R" branch >actual &&
	! grep "multi-1" actual &&
	! grep "multi-2" actual &&
	! grep "multi-3" actual &&
	grep "multi-4" actual
'

test_expect_success 'branch with slash in name' '
	git -C "$R" branch feature/test-123 &&
	git -C "$R" branch >actual &&
	grep "feature/test-123" actual
'

test_expect_success 'branch with dots in name' '
	git -C "$R" branch release.1.0 &&
	git -C "$R" branch >actual &&
	grep "release.1.0" actual
'

test_expect_success 'branch --no-track prevents tracking setup' '
	git -C "$R" branch --no-track no-track-br main &&
	test_must_fail git -C "$R" config branch.no-track-br.remote
'

test_expect_success 'manually set tracking via config' '
	git -C "$R" config branch.develop.remote origin &&
	git -C "$R" config branch.develop.merge refs/heads/develop &&
	git -C "$R" config branch.develop.remote >actual &&
	echo "origin" >expect &&
	test_cmp expect actual
'

test_expect_success 'branch -vv shows manually set tracking' '
	git -C "$R" branch -vv >actual &&
	grep "develop" actual
'

test_expect_success 'branch -l is alias for --list' '
	git -C "$R" branch -l >actual &&
	grep "main" actual &&
	grep "develop" actual
'

test_expect_success 'branch --list with pattern filters' '
	git -C "$R" branch --list "multi-*" >actual &&
	grep "multi-4" actual &&
	! grep "main" actual
'

test_done
