#!/bin/sh
# Tests for grit switch: create branches (-c), orphan branches (--orphan),
# detached HEAD (-d), switch back (-), and branch management.

test_description='grit switch: create, orphan, detach, switch-back, branch ops'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: init repo with initial commit' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "init" >file.txt &&
	grit add file.txt &&
	grit commit -m "initial"
	)
'

# ---- basic switch -c ----
test_expect_success 'switch -c creates new branch' '
	(
	cd repo &&
	grit switch -c feature1 &&
	grit branch >branches &&
	grep "feature1" branches
	)
'

test_expect_success 'switch -c starts on current HEAD' '
	(
	cd repo &&
	head_oid=$(grit rev-parse HEAD) &&
	branch_oid=$(grit rev-parse feature1) &&
	test "$head_oid" = "$branch_oid"
	)
'

test_expect_success 'commit on new branch advances it' '
	(
	cd repo &&
	echo "feature" >feature.txt &&
	grit add feature.txt &&
	grit commit -m "feature commit" &&
	grit log --oneline | grep "feature commit"
	)
'

# ---- switch back to main ----
test_expect_success 'switch to existing branch' '
	(
	cd repo &&
	grit switch main &&
	head_ref=$(grit symbolic-ref HEAD) &&
	test "$head_ref" = "refs/heads/main"
	)
'

test_expect_success 'feature file not present on main' '
	(
	cd repo &&
	! test -f feature.txt
	)
'

# ---- switch - (previous branch) ----
test_expect_success 'switch - goes back to previous branch' '
	(
	cd repo &&
	grit switch - &&
	head_ref=$(grit symbolic-ref HEAD) &&
	test "$head_ref" = "refs/heads/feature1"
	)
'

test_expect_success 'switch - again returns to main' '
	(
	cd repo &&
	grit switch - &&
	head_ref=$(grit symbolic-ref HEAD) &&
	test "$head_ref" = "refs/heads/main"
	)
'

# ---- orphan branch ----
test_expect_success 'switch --orphan creates orphan branch' '
	(
	cd repo &&
	grit switch --orphan orphan1 &&
	head_ref=$(grit symbolic-ref HEAD) &&
	test "$head_ref" = "refs/heads/orphan1"
	)
'

test_expect_success 'orphan branch has no commits' '
	(
	cd repo &&
	test_must_fail grit rev-parse HEAD 2>err
	)
'

test_expect_success 'orphan branch starts with empty index' '
	(
	cd repo &&
	grit ls-files >idx &&
	test ! -s idx
	)
'

test_expect_success 'can commit on orphan branch' '
	(
	cd repo &&
	echo "orphan content" >orphan.txt &&
	grit add orphan.txt &&
	grit commit -m "orphan root" &&
	grit log --oneline >log &&
	test $(wc -l <log) -eq 1
	)
'

test_expect_success 'orphan branch has no common ancestor with main' '
	(
	cd repo &&
	test_must_fail grit merge-base main orphan1 2>err
	)
'

# ---- detached HEAD ----
test_expect_success 'switch -d detaches HEAD at commit' '
	(
	cd repo &&
	grit switch main &&
	head_oid=$(grit rev-parse HEAD) &&
	grit switch -d HEAD &&
	detached_oid=$(grit rev-parse HEAD) &&
	test "$head_oid" = "$detached_oid"
	)
'

test_expect_success 'HEAD is detached (not a symbolic ref)' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref HEAD 2>err
	)
'

test_expect_success 'switch back to named branch from detached' '
	(
	cd repo &&
	grit switch main &&
	head_ref=$(grit symbolic-ref HEAD) &&
	test "$head_ref" = "refs/heads/main"
	)
'

# ---- multiple branches ----
test_expect_success 'create multiple branches and switch between them' '
	(
	cd repo &&
	grit switch -c branch_a &&
	echo "a" >a_file.txt &&
	grit add a_file.txt &&
	grit commit -m "branch a" &&
	grit switch -c branch_b &&
	echo "b" >b_file.txt &&
	grit add b_file.txt &&
	grit commit -m "branch b" &&
	grit switch branch_a &&
	test -f a_file.txt &&
	! test -f b_file.txt
	)
'

test_expect_success 'switch to branch_b shows its files' '
	(
	cd repo &&
	grit switch branch_b &&
	test -f b_file.txt
	)
'

# ---- switch with uncommitted changes ----
test_expect_success 'switch carries non-conflicting worktree changes' '
	(
	cd repo &&
	grit switch main &&
	echo "dirty" >file.txt &&
	grit switch feature1 &&
	test "$(cat file.txt)" = "dirty"
	)
'

test_expect_success 'clean up and go back to main' '
	(
	cd repo &&
	grit restore file.txt &&
	grit switch main
	)
'

# ---- switch -c from specific commit ----
test_expect_success 'switch -c branch from specific commit' '
	(
	cd repo &&
	first=$(grit rev-list --reverse HEAD | head -1) &&
	grit switch -c from_first "$first" &&
	head_oid=$(grit rev-parse HEAD) &&
	test "$head_oid" = "$first"
	)
'

# ---- switch nonexistent branch fails ----
test_expect_success 'switch to nonexistent branch fails' '
	(
	cd repo &&
	test_must_fail grit switch nosuchbranch 2>err
	)
'

# ---- orphan then switch away ----
test_expect_success 'switch away from orphan to named branch' '
	(
	cd repo &&
	grit switch --orphan temp_orphan &&
	grit switch main &&
	head_ref=$(grit symbolic-ref HEAD) &&
	test "$head_ref" = "refs/heads/main"
	)
'

# ---- branch listing ----
test_expect_success 'branch lists all created branches' '
	(
	cd repo &&
	grit branch >list &&
	grep "main" list &&
	grep "feature1" list &&
	grep "orphan1" list &&
	grep "branch_a" list &&
	grep "branch_b" list
	)
'

# ---- switch -c when branch exists fails ----
test_expect_success 'switch -c fails if branch already exists' '
	(
	cd repo &&
	test_must_fail grit switch -c main 2>err
	)
'

# ---- detach at tag-like ref ----
test_expect_success 'switch -d at tagged commit' '
	(
	cd repo &&
	grit tag v1.0 &&
	grit switch -d v1.0 &&
	tag_oid=$(grit rev-parse v1.0) &&
	head_oid=$(grit rev-parse HEAD) &&
	test "$tag_oid" = "$head_oid"
	)
'

test_expect_success 'switch back to main after detach at tag' '
	(
	cd repo &&
	grit switch main &&
	head_ref=$(grit symbolic-ref HEAD) &&
	test "$head_ref" = "refs/heads/main"
	)
'

# ---- orphan preserves no worktree from previous branch ----
test_expect_success 'orphan branch clears worktree from previous branch' '
	(
	cd repo &&
	grit switch --orphan clean_orphan &&
	grit ls-files >idx &&
	test ! -s idx
	)
'

# ---- switch -c with dirty compatible changes ----
test_expect_success 'switch -c carries compatible uncommitted changes' '
	(
	cd repo &&
	grit switch main &&
	echo "new_content" >new_uncommitted.txt &&
	grit add new_uncommitted.txt &&
	grit switch -c carry_branch &&
	grit ls-files --error-unmatch new_uncommitted.txt &&
	test -f new_uncommitted.txt
	)
'

test_expect_success 'commit carried changes on new branch' '
	(
	cd repo &&
	grit commit -m "carried changes" &&
	grit log --oneline | grep "carried changes"
	)
'

test_done
