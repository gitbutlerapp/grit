#!/bin/sh
# Tests for grit switch: -c/--create, --detach, --orphan,
# --discard-changes, and switching between branches.

test_description='grit switch --create, --detach, --orphan, and branch switching'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with main and commits' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@example.com" &&
	git config user.name "Test User" &&
	echo "file1" >file1.txt &&
	grit add file1.txt &&
	grit commit -m "first commit" &&
	echo "file2" >file2.txt &&
	grit add file2.txt &&
	grit commit -m "second commit" &&
	echo "file3" >file3.txt &&
	grit add file3.txt &&
	grit commit -m "third commit"
	)
'

# --- basic branch switching ---

test_expect_success 'switch -c creates and switches to new branch' '
	(
	cd repo &&
	grit switch -c feature1 &&
	grit branch >branches &&
	grep "feature1" branches &&
	grit symbolic-ref HEAD >head &&
	grep "refs/heads/feature1" head
	)
'

test_expect_success 'switch back to main' '
	(
	cd repo &&
	grit switch main &&
	grit symbolic-ref HEAD >head &&
	grep "refs/heads/main" head
	)
'

test_expect_success 'switch --create creates new branch (long form)' '
	(
	cd repo &&
	grit switch --create feature2 &&
	grit symbolic-ref HEAD >head &&
	grep "refs/heads/feature2" head
	)
'

test_expect_success 'switch to existing branch' '
	(
	cd repo &&
	grit switch feature1 &&
	grit symbolic-ref HEAD >head &&
	grep "refs/heads/feature1" head
	)
'

test_expect_success 'switch -c from a non-default branch' '
	(
	cd repo &&
	grit switch -c sub-feature &&
	grit symbolic-ref HEAD >head &&
	grep "refs/heads/sub-feature" head
	)
'

test_expect_success 'switch - goes back to previous branch' '
	(
	cd repo &&
	grit switch main &&
	grit switch feature1 &&
	grit switch - &&
	grit symbolic-ref HEAD >head &&
	grep "refs/heads/main" head
	)
'

# --- create branch from specific start point ---

test_expect_success 'switch -c with start point creates branch at that commit' '
	(
	cd repo &&
	grit switch main &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	PARENT_OID=$(grit rev-parse HEAD~1) &&
	grit switch -c from-parent HEAD~1 &&
	BRANCH_OID=$(grit rev-parse HEAD) &&
	test "$BRANCH_OID" = "$PARENT_OID"
	)
'

test_expect_success 'branch from start point does not have later files' '
	(
	cd repo &&
	! test -f file3.txt &&
	test -f file1.txt &&
	test -f file2.txt
	)
'

test_expect_success 'switch back to main has all files' '
	(
	cd repo &&
	grit switch main &&
	test -f file1.txt &&
	test -f file2.txt &&
	test -f file3.txt
	)
'

# --- duplicate branch name ---

test_expect_success 'switch -c fails if branch already exists' '
	(
	cd repo &&
	test_must_fail grit switch -c feature1 2>err &&
	grep -i "already exists" err
	)
'

test_expect_success 'current branch unchanged after failed switch -c' '
	(
	cd repo &&
	grit symbolic-ref HEAD >head &&
	grep "refs/heads/main" head
	)
'

# --- detach ---

test_expect_success 'switch --detach HEAD puts us in detached HEAD' '
	(
	cd repo &&
	grit switch --detach HEAD &&
	test_must_fail grit symbolic-ref HEAD 2>err
	)
'

test_expect_success 'switch --detach to specific commit' '
	(
	cd repo &&
	PARENT=$(grit rev-parse main~1) &&
	grit switch --detach main~1 &&
	ACTUAL=$(grit rev-parse HEAD) &&
	test "$ACTUAL" = "$PARENT"
	)
'

test_expect_success 'switch -d is short for --detach' '
	(
	cd repo &&
	grit switch -d main &&
	test_must_fail grit symbolic-ref HEAD 2>err &&
	ACTUAL=$(grit rev-parse HEAD) &&
	EXPECTED=$(grit rev-parse main) &&
	test "$ACTUAL" = "$EXPECTED"
	)
'

test_expect_success 'can switch back to branch from detached' '
	(
	cd repo &&
	grit switch main &&
	grit symbolic-ref HEAD >head &&
	grep "refs/heads/main" head
	)
'

# --- orphan ---

test_expect_success 'switch --orphan creates orphan branch' '
	(
	cd repo &&
	grit switch --orphan orphan1 &&
	grit symbolic-ref HEAD >head &&
	grep "refs/heads/orphan1" head
	)
'

test_expect_success 'orphan branch has empty index' '
	(
	cd repo &&
	grit ls-files >ls_out &&
	test ! -s ls_out
	)
'

test_expect_success 'can commit on orphan branch' '
	(
	cd repo &&
	echo "orphan-file" >orphan.txt &&
	grit add orphan.txt &&
	grit commit -m "orphan commit" &&
	grit log --oneline >log_out &&
	grep "orphan commit" log_out
	)
'

test_expect_success 'orphan commit has no parents' '
	(
	cd repo &&
	PARENTS=$(grit rev-list --parents -1 HEAD) &&
	# should be just the commit hash, no parent hash
	test "$(echo "$PARENTS" | wc -w)" -eq 1
	)
'

test_expect_success 'switch back to main after orphan' '
	(
	cd repo &&
	grit switch main &&
	test -f file1.txt &&
	test -f file2.txt &&
	test -f file3.txt
	)
'

# --- dirty worktree interactions ---

test_expect_success 'switch fails with uncommitted changes that conflict' '
	(
	cd repo &&
	echo "dirty" >>file1.txt &&
	grit add file1.txt &&
	grit switch -c has-dirty &&
	echo "more" >>file1.txt &&
	grit commit -a -m "dirty on has-dirty" &&
	grit switch main &&
	echo "conflict" >>file1.txt &&
	test_must_fail grit switch has-dirty 2>err
	)
'

test_expect_success 'switch --discard-changes overrides dirty worktree' '
	(
	cd repo &&
	grit switch --discard-changes has-dirty &&
	grit symbolic-ref HEAD >head &&
	grep "refs/heads/has-dirty" head
	)
'

test_expect_success 'discarded changes are gone' '
	(
	cd repo &&
	! grep "conflict" file1.txt
	)
'

test_expect_success 'switch to nonexistent branch fails' '
	(
	cd repo &&
	test_must_fail grit switch no-such-branch 2>err
	)
'

test_expect_success 'switch with no arguments fails' '
	(
	cd repo &&
	test_must_fail grit switch 2>err
	)
'

# --- switch preserves untracked files ---

test_expect_success 'switch preserves untracked files' '
	(
	cd repo &&
	grit switch main &&
	echo "untracked-data" >untracked.txt &&
	grit switch feature1 &&
	test -f untracked.txt &&
	test "$(cat untracked.txt)" = "untracked-data"
	)
'

test_expect_success 'switch -c preserves untracked files' '
	(
	cd repo &&
	grit switch main &&
	echo "untracked2" >ut2.txt &&
	grit switch -c new-with-untracked &&
	test -f ut2.txt
	)
'

# --- multiple create and switch round-trips ---

test_expect_success 'switch -c with tag as start point' '
	(
	cd repo &&
	grit switch main &&
	grit tag v1.0 HEAD~1 &&
	grit switch -c from-tag v1.0 &&
	ACTUAL=$(grit rev-parse HEAD) &&
	EXPECTED=$(grit rev-parse v1.0) &&
	test "$ACTUAL" = "$EXPECTED"
	)
'

test_expect_success 'create multiple branches and round-trip between them' '
	(
	cd repo &&
	grit switch main &&
	grit switch -c br-a &&
	echo "a-content" >a-only.txt &&
	grit add a-only.txt &&
	grit commit -m "branch a file" &&
	grit switch main &&
	grit switch -c br-b &&
	echo "b-content" >b-only.txt &&
	grit add b-only.txt &&
	grit commit -m "branch b file" &&
	grit switch br-a &&
	test -f a-only.txt &&
	! test -f b-only.txt &&
	grit switch br-b &&
	test -f b-only.txt &&
	! test -f a-only.txt
	)
'

test_done
