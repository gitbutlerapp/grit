#!/bin/sh
# Tests for diff-tree with recursive mode and various options.

test_description='diff-tree recursive'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- setup ---------------------------------------------------------------------

test_expect_success 'setup repository with nested trees' '
	(
	git init repo &&
	cd repo &&
	mkdir -p a/b/c &&
	echo "root file" >root.txt &&
	echo "level a" >a/file.txt &&
	echo "level b" >a/b/file.txt &&
	echo "level c" >a/b/c/file.txt &&
	git add . &&
	git commit -m "initial" &&
	echo "modified root" >root.txt &&
	echo "modified c" >a/b/c/file.txt &&
	git add . &&
	git commit -m "modify root and deep file"
	)
'

# -- basic recursive diff-tree -------------------------------------------------

test_expect_success 'diff-tree -r shows all changed blobs' '
	(
	cd repo &&
	git diff-tree -r HEAD~1 HEAD >out &&
	grep "root.txt" out &&
	grep "a/b/c/file.txt" out
	)
'

test_expect_success 'diff-tree -r does not show unchanged files' '
	(
	cd repo &&
	git diff-tree -r HEAD~1 HEAD >out &&
	! grep "a/file.txt" out &&
	! grep "a/b/file.txt" out
	)
'

test_expect_success 'diff-tree without -r shows only top-level tree changes' '
	(
	cd repo &&
	git diff-tree HEAD~1 HEAD >out &&
	grep "root.txt" out
	)
'

test_expect_success 'diff-tree -r with single commit shows parent diff' '
	(
	cd repo &&
	git diff-tree -r HEAD >out &&
	grep "root.txt" out &&
	grep "a/b/c/file.txt" out
	)
'

# -- added and deleted files ---------------------------------------------------

test_expect_success 'setup: add new nested file' '
	(
	cd repo &&
	mkdir -p d/e &&
	echo "new deep" >d/e/new.txt &&
	git add . &&
	git commit -m "add deep file"
	)
'

test_expect_success 'diff-tree -r shows added file with A status' '
	(
	cd repo &&
	git diff-tree -r HEAD~1 HEAD >out &&
	grep "A" out | grep "d/e/new.txt"
	)
'

test_expect_success 'setup: delete a nested file' '
	(
	cd repo &&
	git rm a/b/c/file.txt &&
	git commit -m "delete deep file"
	)
'

test_expect_success 'diff-tree -r shows deleted file with D status' '
	(
	cd repo &&
	git diff-tree -r HEAD~1 HEAD >out &&
	grep "D" out | grep "a/b/c/file.txt"
	)
'

# -- name-only and name-status -------------------------------------------------

test_expect_success 'diff-tree -r --name-only lists paths' '
	(
	cd repo &&
	git diff-tree -r --name-only HEAD~1 HEAD >out &&
	grep "a/b/c/file.txt" out &&
	! grep "^:" out
	)
'

test_expect_success 'diff-tree -r --name-status shows status letter and path' '
	(
	cd repo &&
	git diff-tree -r --name-status HEAD~1 HEAD >out &&
	grep "^D" out | grep "a/b/c/file.txt"
	)
'

# -- diff-tree with root commit ------------------------------------------------

test_expect_success 'diff-tree -r --root shows initial commit additions' '
	(
	cd repo &&
	initial=$(git log --reverse --format=%H | head -1) &&
	git diff-tree -r --root "$initial" >out &&
	grep "root.txt" out &&
	grep "a/file.txt" out &&
	grep "a/b/file.txt" out &&
	grep "a/b/c/file.txt" out
	)
'

test_expect_success 'diff-tree --root shows A status for all initial files' '
	(
	cd repo &&
	initial=$(git log --reverse --format=%H | head -1) &&
	git diff-tree -r --root --name-status "$initial" >out &&
	grep "^A" out
	)
'

# -- multiple trees and path filtering -----------------------------------------

test_expect_success 'diff-tree -r with path filter restricts output' '
	(
	cd repo &&
	git diff-tree -r HEAD~3 HEAD~2 -- a/b/c/ >out &&
	grep "a/b/c/file.txt" out &&
	! grep "root.txt" out
	)
'

test_expect_success 'diff-tree -r with path filter for root file' '
	(
	cd repo &&
	git diff-tree -r HEAD~3 HEAD~2 -- root.txt >out &&
	grep "root.txt" out &&
	! grep "a/" out
	)
'

# -- diff-tree comparing same tree ---------------------------------------------

test_expect_success 'diff-tree -r same commit produces no output' '
	(
	cd repo &&
	git diff-tree -r HEAD HEAD >out &&
	test_line_count = 0 out
	)
'

# -- diff-tree -t (show tree entries) -----------------------------------------

test_expect_success 'diff-tree -r -t shows tree entries alongside blobs' '
	(
	cd repo &&
	git diff-tree -r -t HEAD~3 HEAD~2 >out &&
	grep "root.txt" out
	)
'

# -- new repo with renames for diff-tree ---------------------------------------

test_expect_success 'setup: repo with renames' '
	(
	git init rename-repo &&
	cd rename-repo &&
	echo "content" >original.txt &&
	mkdir sub &&
	echo "sub content" >sub/file.txt &&
	git add . &&
	git commit -m "initial" &&
	git mv original.txt renamed.txt &&
	git commit -m "rename file"
	)
'

test_expect_success 'diff-tree -r shows rename as add+delete' '
	(
	cd rename-repo &&
	git diff-tree -r HEAD~1 HEAD >out &&
	grep "original.txt" out &&
	grep "renamed.txt" out
	)
'

test_expect_success 'diff-tree -r rename shows D status for original' '
	(
	cd rename-repo &&
	git diff-tree -r --name-status HEAD~1 HEAD >out &&
	grep "^D" out | grep "original.txt"
	)
'

# -- diff-tree with copies ----------------------------------------------------

test_expect_success 'setup: copy scenario' '
	(
	cd rename-repo &&
	cp renamed.txt copied.txt &&
	git add copied.txt &&
	git commit -m "copy file"
	)
'

test_expect_success 'diff-tree -r shows copy as addition' '
	(
	cd rename-repo &&
	git diff-tree -r HEAD~1 HEAD >out &&
	grep "copied.txt" out
	)
'

# -- diff-tree --diff-filter --------------------------------------------------

test_expect_success 'diff-tree --diff-filter=A shows only additions' '
	(
	cd rename-repo &&
	git diff-tree -r --diff-filter=A HEAD~1 HEAD >out &&
	grep "copied.txt" out
	)
'

test_expect_success 'diff-tree --diff-filter=D shows only deletions' '
	(
	cd repo &&
	git diff-tree -r --diff-filter=D HEAD~1 HEAD >out &&
	grep "a/b/c/file.txt" out
	)
'

test_expect_success 'diff-tree --diff-filter=M shows only modifications' '
	(
	cd repo &&
	git diff-tree -r --diff-filter=M HEAD~3 HEAD~2 >out &&
	grep "root.txt" out &&
	grep "a/b/c/file.txt" out
	)
'

# -- output format flags -------------------------------------------------------

test_expect_success 'diff-tree -r --raw produces raw output' '
	(
	cd repo &&
	git diff-tree -r HEAD~3 HEAD~2 >out &&
	grep "^:" out
	)
'

test_expect_success 'diff-tree -r -p produces patch output' '
	(
	cd repo &&
	git diff-tree -r -p HEAD~3 HEAD~2 >out &&
	grep "^diff --git" out &&
	grep "^@@" out
	)
'

test_expect_success 'diff-tree -r --stat produces stat output' '
	(
	cd repo &&
	git diff-tree -r --stat HEAD~3 HEAD~2 >out &&
	grep "root.txt" out &&
	grep "changed" out
	)
'

test_done
