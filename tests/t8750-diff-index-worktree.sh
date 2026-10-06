#!/bin/sh
# Tests for diff-index comparing index to worktree and HEAD.

test_description='diff-index worktree comparisons'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- setup ---------------------------------------------------------------------

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	echo "first" >file1.txt &&
	echo "second" >file2.txt &&
	echo "third" >file3.txt &&
	mkdir sub &&
	echo "nested" >sub/deep.txt &&
	git add . &&
	git commit -m "initial commit"
	)
'

# -- diff-index HEAD (cached vs HEAD) -----------------------------------------

test_expect_success 'diff-index HEAD with clean worktree shows nothing' '
	(
	cd repo &&
	git diff-index HEAD >out &&
	test_line_count = 0 out
	)
'

test_expect_success 'diff-index HEAD after staging shows changes' '
	(
	cd repo &&
	echo "modified" >file1.txt &&
	git add file1.txt &&
	git diff-index HEAD >out &&
	grep "file1.txt" out &&
	! grep "file2.txt" out
	)
'

test_expect_success 'diff-index --cached HEAD shows staged changes' '
	(
	cd repo &&
	git diff-index --cached HEAD >out &&
	grep "file1.txt" out
	)
'

test_expect_success 'diff-index --cached HEAD does not show unstaged changes' '
	(
	cd repo &&
	echo "worktree only" >file2.txt &&
	git diff-index --cached HEAD >out &&
	grep "file1.txt" out &&
	! grep "file2.txt" out
	)
'

test_expect_success 'diff-index HEAD (without --cached) shows staged and unstaged' '
	(
	cd repo &&
	git diff-index HEAD >out &&
	grep "file1.txt" out &&
	grep "file2.txt" out
	)
'

# -- commit and reset state ----------------------------------------------------

test_expect_success 'commit staged changes and verify clean state' '
	(
	cd repo &&
	git checkout -- file2.txt &&
	git commit -m "modify file1" &&
	git diff-index --cached HEAD >out &&
	test_line_count = 0 out
	)
'

# -- diff-index with new files -------------------------------------------------

test_expect_success 'diff-index HEAD shows newly added file' '
	(
	cd repo &&
	echo "brand new" >newfile.txt &&
	git add newfile.txt &&
	git diff-index --cached HEAD >out &&
	grep "newfile.txt" out
	)
'

test_expect_success 'diff-index shows A status for new file in raw output' '
	(
	cd repo &&
	git diff-index --cached HEAD >out &&
	grep "A" out | grep "newfile.txt"
	)
'

# -- diff-index with deleted files ---------------------------------------------

test_expect_success 'diff-index HEAD shows deleted file' '
	(
	cd repo &&
	git rm file3.txt &&
	git diff-index --cached HEAD >out &&
	grep "file3.txt" out
	)
'

test_expect_success 'diff-index shows D status for deleted file in raw output' '
	(
	cd repo &&
	git diff-index --cached HEAD >out &&
	grep "D" out | grep "file3.txt"
	)
'

test_expect_success 'cleanup: commit deletions and additions' '
	(
	cd repo &&
	git commit -m "add new, remove file3"
	)
'

# -- diff-index raw format output ----------------------------------------------

test_expect_success 'setup: stage some changes' '
	(
	cd repo &&
	echo "changed" >file1.txt &&
	echo "also changed" >sub/deep.txt &&
	git add .
	)
'

test_expect_success 'diff-index --cached raw output starts with colon' '
	(
	cd repo &&
	git diff-index --cached HEAD >out &&
	grep "^:" out
	)
'

test_expect_success 'diff-index --cached raw output contains mode info' '
	(
	cd repo &&
	git diff-index --cached HEAD >out &&
	grep "100644" out
	)
'

test_expect_success 'diff-index --cached raw output contains M status for modifications' '
	(
	cd repo &&
	git diff-index --cached HEAD >out &&
	grep "M" out | grep "file1.txt" &&
	grep "M" out | grep "sub/deep.txt"
	)
'

test_expect_success 'cleanup: commit staged changes' '
	(
	cd repo &&
	git commit -m "modify multiple files"
	)
'

# -- diff-index with path limiter ----------------------------------------------

test_expect_success 'setup: modify multiple files' '
	(
	cd repo &&
	echo "path-limited change" >file1.txt &&
	echo "also modified" >file2.txt &&
	git add .
	)
'

test_expect_success 'diff-index --cached HEAD -- path limits output' '
	(
	cd repo &&
	git diff-index --cached HEAD -- file1.txt >out &&
	grep "file1.txt" out &&
	! grep "file2.txt" out
	)
'

test_expect_success 'diff-index --cached HEAD -- subdir limits to subdir' '
	(
	cd repo &&
	echo "deep change" >sub/deep.txt &&
	git add sub/deep.txt &&
	git diff-index --cached HEAD -- sub/ >out &&
	grep "sub/deep.txt" out &&
	! grep "file1.txt" out
	)
'

test_expect_success 'cleanup: commit path filter test' '
	(
	cd repo &&
	git commit -m "more modifications"
	)
'

# -- diff-index comparing to older commits ------------------------------------

test_expect_success 'diff-index --cached against older commit shows all differences' '
	(
	cd repo &&
	git diff-index --cached HEAD~2 >out &&
	grep "file1.txt" out
	)
'

# -- diff-index --exit-code ----------------------------------------------------

test_expect_success 'diff-index --exit-code returns 0 on clean state' '
	(
	cd repo &&
	git diff-index --cached --exit-code HEAD
	)
'

test_expect_success 'diff-index --exit-code returns non-zero on changes' '
	(
	cd repo &&
	echo "exit code test" >file1.txt &&
	git add file1.txt &&
	test_must_fail git diff-index --cached --exit-code HEAD
	)
'

# -- diff-index --quiet --------------------------------------------------------

test_expect_success 'diff-index --quiet suppresses output but exits non-zero' '
	(
	cd repo &&
	git diff-index --cached --quiet HEAD >out 2>&1 &&
	false || test_line_count = 0 out
	)
'

test_expect_success 'diff-index --quiet on clean state exits 0' '
	(
	cd repo &&
	git commit -m "for quiet test" &&
	git diff-index --cached --quiet HEAD
	)
'

# -- diff-index --abbrev -------------------------------------------------------

test_expect_success 'diff-index --abbrev shortens OIDs' '
	(
	cd repo &&
	echo "abbrev test" >file1.txt &&
	git add file1.txt &&
	git diff-index --cached --abbrev HEAD >out &&
	grep "file1.txt" out
	)
'

test_expect_success 'diff-index --abbrev=7 produces 7-char OIDs' '
	(
	cd repo &&
	git diff-index --cached --abbrev=7 HEAD >out &&
	grep "file1.txt" out
	)
'

test_expect_success 'cleanup abbrev test' '
	(
	cd repo &&
	git commit -m "abbrev test commit"
	)
'

# -- diff-index without --cached (worktree vs HEAD) ---------------------------

test_expect_success 'diff-index HEAD detects worktree modification' '
	(
	cd repo &&
	echo "worktree mod" >file1.txt &&
	git diff-index HEAD >out &&
	grep "file1.txt" out
	)
'

test_expect_success 'diff-index HEAD does not detect staged-only changes via worktree path' '
	(
	cd repo &&
	git add file1.txt &&
	echo "another worktree mod" >file2.txt &&
	git diff-index HEAD >out &&
	grep "file1.txt" out &&
	grep "file2.txt" out
	)
'

test_expect_success 'diff-index --cached does not show worktree-only changes' '
	(
	cd repo &&
	git diff-index --cached HEAD >out &&
	grep "file1.txt" out &&
	! grep "file2.txt" out
	)
'

test_expect_success 'cleanup: reset worktree state' '
	(
	cd repo &&
	git checkout -- file2.txt &&
	git commit -m "worktree test commit"
	)
'

# -- diff-index on empty staging area ------------------------------------------

test_expect_success 'diff-index --cached HEAD on clean repo is empty' '
	(
	cd repo &&
	git diff-index --cached HEAD >out &&
	test_line_count = 0 out
	)
'

test_done
