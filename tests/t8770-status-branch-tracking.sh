#!/bin/sh
# Tests for status with branch display, short/porcelain formats.

test_description='status branch and tracking display'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# Use a shared output dir outside the repo to avoid polluting worktree
OUT="$TRASH_DIRECTORY/output"
mkdir -p "$OUT"

# -- setup ---------------------------------------------------------------------

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	echo "initial" >file1.txt &&
	echo "second" >file2.txt &&
	mkdir sub &&
	echo "nested" >sub/deep.txt &&
	git add . &&
	git commit -m "initial commit"
	)
'

# -- long format: clean --------------------------------------------------------

test_expect_success 'status on clean worktree mentions branch' '
	(
	cd repo &&
	git status >"$OUT/s1" &&
	grep "On branch master" "$OUT/s1"
	)
'

test_expect_success 'status on clean worktree says working tree clean' '
	(
	cd repo &&
	git status >"$OUT/s2" &&
	grep "working tree clean" "$OUT/s2"
	)
'

# -- short format basics -------------------------------------------------------

test_expect_success 'status -s on clean worktree shows no output' '
	(
	cd repo &&
	git status -s >"$OUT/s3" &&
	test_line_count = 0 "$OUT/s3"
	)
'

test_expect_success 'status --short on clean worktree shows no output' '
	(
	cd repo &&
	git status --short >"$OUT/s4" &&
	test_line_count = 0 "$OUT/s4"
	)
'

# -- modified files in short format --------------------------------------------

test_expect_success 'status -s shows worktree modification with space-M prefix' '
	(
	cd repo &&
	echo "modified" >file1.txt &&
	git status -s >"$OUT/s5" &&
	grep "^ M file1.txt" "$OUT/s5"
	)
'

test_expect_success 'status -s shows staged modification with M-space prefix' '
	(
	cd repo &&
	git add file1.txt &&
	git status -s >"$OUT/s6" &&
	grep "^M  file1.txt" "$OUT/s6"
	)
'

test_expect_success 'status -s shows both staged and unstaged modifications' '
	(
	cd repo &&
	echo "more changes" >file1.txt &&
	git status -s >"$OUT/s7" &&
	grep "^MM file1.txt" "$OUT/s7"
	)
'

# -- untracked files -----------------------------------------------------------

test_expect_success 'status -s shows untracked files with ??' '
	(
	cd repo &&
	echo "new" >untracked.txt &&
	git status -s >"$OUT/s8" &&
	grep "^?? untracked.txt" "$OUT/s8"
	)
'

test_expect_success 'status -s shows untracked directory entry' '
	(
	cd repo &&
	mkdir newdir &&
	echo "x" >newdir/file.txt &&
	git status -s >"$OUT/s9" &&
	grep "^??" "$OUT/s9" | grep "newdir"
	)
'

test_expect_success 'status -u no hides untracked files' '
	(
	cd repo &&
	git status -s -u no >"$OUT/s10" &&
	! grep "^??" "$OUT/s10"
	)
'

test_expect_success 'cleanup untracked' '
	(
	cd repo &&
	rm -rf untracked.txt newdir
	)
'

# -- added files ---------------------------------------------------------------

test_expect_success 'status -s shows newly added file with A prefix' '
	(
	cd repo &&
	echo "brand new" >added.txt &&
	git add added.txt &&
	git status -s >"$OUT/s11" &&
	grep "^A  added.txt" "$OUT/s11"
	)
'

# -- deleted files -------------------------------------------------------------

test_expect_success 'status -s shows staged deletion with D prefix' '
	(
	cd repo &&
	git rm file2.txt &&
	git status -s >"$OUT/s12" &&
	grep "^D  file2.txt" "$OUT/s12"
	)
'

test_expect_success 'cleanup: commit add/delete' '
	(
	cd repo &&
	git checkout -- file1.txt &&
	git commit -m "add and delete"
	)
'

# -- worktree-only deletion (not staged) ---------------------------------------

test_expect_success 'status -s shows worktree deletion with space-D prefix' '
	(
	cd repo &&
	rm sub/deep.txt &&
	git status -s >"$OUT/s13" &&
	grep "^ D sub/deep.txt" "$OUT/s13"
	)
'

test_expect_success 'restore worktree deletion' '
	(
	cd repo &&
	git checkout -- sub/deep.txt
	)
'

# -- porcelain format ----------------------------------------------------------

test_expect_success 'status --porcelain clean repo has no ## line (matches git)' '
	(
	cd repo &&
	git status --porcelain >"$OUT/s14" &&
	test_line_count = 0 "$OUT/s14"
	)
'

test_expect_success 'status --porcelain shows modifications' '
	(
	cd repo &&
	echo "porcelain test" >file1.txt &&
	git status --porcelain >"$OUT/s15" &&
	grep "file1.txt" "$OUT/s15"
	)
'

test_expect_success 'status --porcelain with clean worktree is empty (matches git)' '
	(
	cd repo &&
	git checkout -- file1.txt &&
	git status --porcelain >"$OUT/s16" &&
	test_line_count = 0 "$OUT/s16"
	)
'

# -- branch flag in short mode -------------------------------------------------

test_expect_success 'status -sb shows branch line' '
	(
	cd repo &&
	git status -sb >"$OUT/s17" &&
	grep "^## master" "$OUT/s17"
	)
'

test_expect_success 'status -s does not show branch line' '
	(
	cd repo &&
	git status -s >"$OUT/s18" &&
	! grep "^##" "$OUT/s18"
	)
'

# -- multiple simultaneous states ----------------------------------------------

test_expect_success 'setup: create various status states simultaneously' '
	(
	cd repo &&
	echo "staged mod" >file1.txt &&
	git add file1.txt &&
	echo "new staged" >staged-new.txt &&
	git add staged-new.txt &&
	echo "untracked" >untrack-me.txt &&
	rm sub/deep.txt
	)
'

test_expect_success 'status -s shows all state types at once' '
	(
	cd repo &&
	git status -s >"$OUT/s19" &&
	grep "^M  file1.txt" "$OUT/s19" &&
	grep "^A  staged-new.txt" "$OUT/s19" &&
	grep "^?? untrack-me.txt" "$OUT/s19" &&
	grep "^ D sub/deep.txt" "$OUT/s19"
	)
'

test_expect_success 'status long format shows changes to be committed section' '
	(
	cd repo &&
	git status >"$OUT/s20" &&
	grep "Changes to be committed" "$OUT/s20"
	)
'

test_expect_success 'cleanup: restore and commit' '
	(
	cd repo &&
	git checkout -- sub/deep.txt &&
	rm untrack-me.txt &&
	git commit -m "multiple states cleanup"
	)
'

# -- .gitignore handling -------------------------------------------------------

test_expect_success 'setup: create .gitignore' '
	(
	cd repo &&
	echo "*.log" >.gitignore &&
	git add .gitignore &&
	git commit -m "add gitignore"
	)
'

test_expect_success 'status long format respects .gitignore for display' '
	(
	cd repo &&
	echo "debug info" >debug.log &&
	git status >"$OUT/s21" &&
	grep -i "ignored\|untracked\|debug.log\|nothing" "$OUT/s21"
	)
'

test_expect_success 'cleanup ignored' '
	(
	cd repo &&
	rm -f debug.log
	)
'

# -- status on new branch -----------------------------------------------------

test_expect_success 'status shows new branch name after checkout -b' '
	(
	cd repo &&
	git checkout -b feature-x &&
	git status >"$OUT/s22" &&
	grep "On branch feature-x" "$OUT/s22"
	)
'

test_expect_success 'status -sb shows new branch name' '
	(
	cd repo &&
	git status -sb >"$OUT/s23" &&
	grep "^## feature-x" "$OUT/s23"
	)
'

test_expect_success 'status on branch with new commit shows branch' '
	(
	cd repo &&
	echo "feature work" >feature.txt &&
	git add feature.txt &&
	git commit -m "feature commit" &&
	git status >"$OUT/s24" &&
	grep "On branch feature-x" "$OUT/s24"
	)
'

# -- status after switching back to master ------------------------------------

test_expect_success 'status shows master after switching back' '
	(
	cd repo &&
	git checkout master &&
	git status >"$OUT/s25" &&
	grep "On branch master" "$OUT/s25"
	)
'

test_expect_success 'status -sb shows master after switch' '
	(
	cd repo &&
	git status -sb >"$OUT/s26" &&
	grep "^## master" "$OUT/s26"
	)
'

test_done
