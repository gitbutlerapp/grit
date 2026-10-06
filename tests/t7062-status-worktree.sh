#!/bin/sh
# Tests for grit status with worktree changes (modified, deleted, untracked)

test_description='grit status worktree changes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup base repo' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo "aaa" >a.txt &&
	echo "bbb" >b.txt &&
	echo "ccc" >c.txt &&
	mkdir dir1 &&
	echo "ddd" >dir1/d.txt &&
	mkdir dir2 &&
	echo "eee" >dir2/e.txt &&
	git add . &&
	git commit -m "initial"
	)
'

# === worktree modifications ===

test_expect_success 'status detects single worktree modification' '
	(
	cd repo &&
	echo "modified" >a.txt &&
	git status --porcelain >../actual &&
	grep "^ M a.txt" ../actual &&
	git checkout -- a.txt
	)
'

test_expect_success 'status detects multiple worktree modifications at once' '
	(
	cd repo &&
	echo "mod a" >a.txt &&
	echo "mod b" >b.txt &&
	echo "mod c" >c.txt &&
	git status --porcelain >../actual &&
	grep "^ M a.txt" ../actual &&
	grep "^ M b.txt" ../actual &&
	grep "^ M c.txt" ../actual &&
	git checkout -- .
	)
'

test_expect_success 'status detects modification in subdirectory' '
	(
	cd repo &&
	echo "mod d" >dir1/d.txt &&
	git status --porcelain >../actual &&
	grep "^ M dir1/d.txt" ../actual &&
	git checkout -- dir1/d.txt
	)
'

test_expect_success 'short format shows worktree modification' '
	(
	cd repo &&
	echo "short mod" >a.txt &&
	git status -s >../actual &&
	grep "^ M a.txt" ../actual &&
	git checkout -- a.txt
	)
'

# === worktree deletions ===

test_expect_success 'status detects worktree deletion' '
	(
	cd repo &&
	rm a.txt &&
	git status --porcelain >../actual &&
	grep "^ D a.txt" ../actual &&
	git checkout -- a.txt
	)
'

test_expect_success 'status detects multiple deletions at once' '
	(
	cd repo &&
	rm a.txt b.txt &&
	git status --porcelain >../actual &&
	grep "^ D a.txt" ../actual &&
	grep "^ D b.txt" ../actual &&
	git checkout -- .
	)
'

test_expect_success 'status detects deletion in subdirectory' '
	(
	cd repo &&
	rm dir1/d.txt &&
	git status --porcelain >../actual &&
	grep "^ D dir1/d.txt" ../actual &&
	git checkout -- dir1/d.txt
	)
'

test_expect_success 'short format shows deletion' '
	(
	cd repo &&
	rm c.txt &&
	git status -s >../actual &&
	grep "^ D c.txt" ../actual &&
	git checkout -- c.txt
	)
'

# === untracked files ===

test_expect_success 'status shows untracked file' '
	(
	cd repo &&
	echo "untracked" >untracked.txt &&
	git status --porcelain >../actual &&
	grep "^?? untracked.txt" ../actual &&
	rm -f untracked.txt
	)
'

test_expect_success 'status shows multiple untracked files' '
	(
	cd repo &&
	echo "u2" >u2.txt &&
	echo "u3" >u3.txt &&
	git status --porcelain >../actual &&
	grep "^?? u2.txt" ../actual &&
	grep "^?? u3.txt" ../actual &&
	rm -f u2.txt u3.txt
	)
'

test_expect_success 'status shows untracked file in subdirectory' '
	(
	cd repo &&
	echo "new" >dir1/new.txt &&
	git status --porcelain >../actual &&
	grep "dir1/new.txt" ../actual &&
	rm -f dir1/new.txt
	)
'

test_expect_success 'status shows new untracked directory' '
	(
	cd repo &&
	mkdir newdir &&
	echo "x" >newdir/file.txt &&
	git status --porcelain >../actual &&
	grep "newdir/" ../actual &&
	rm -rf newdir
	)
'

# === untracked-files flag ===

test_expect_success 'untracked-files=no hides untracked' '
	(
	cd repo &&
	echo "hide me" >hidden.txt &&
	git status --porcelain -u no >../actual &&
	! grep "hidden.txt" ../actual &&
	rm hidden.txt
	)
'

test_expect_success 'untracked-files=normal shows untracked' '
	(
	cd repo &&
	echo "show me" >visible.txt &&
	git status --porcelain -u normal >../actual &&
	grep "^?? visible.txt" ../actual &&
	rm visible.txt
	)
'

# === mixed worktree states ===

test_expect_success 'status shows modified and untracked together' '
	(
	cd repo &&
	echo "modified" >a.txt &&
	echo "new" >new.txt &&
	git status --porcelain >../actual &&
	grep "^ M a.txt" ../actual &&
	grep "^?? new.txt" ../actual &&
	git checkout -- a.txt &&
	rm -f new.txt
	)
'

test_expect_success 'status shows deleted and untracked together' '
	(
	cd repo &&
	rm b.txt &&
	echo "new2" >new2.txt &&
	git status --porcelain >../actual &&
	grep "^ D b.txt" ../actual &&
	grep "^?? new2.txt" ../actual &&
	git checkout -- b.txt &&
	rm -f new2.txt
	)
'

test_expect_success 'status shows modified, deleted, and untracked together' '
	(
	cd repo &&
	echo "mod a" >a.txt &&
	rm b.txt &&
	echo "new3" >new3.txt &&
	git status -s >../actual &&
	grep " M" ../actual &&
	grep " D" ../actual &&
	grep "??" ../actual &&
	git checkout -- . &&
	rm -f new3.txt
	)
'

test_expect_success 'porcelain with branch for mixed state' '
	(
	cd repo &&
	echo "mod" >a.txt &&
	echo "u" >u.txt &&
	git status --porcelain -b >../actual &&
	head -1 ../actual | grep "##" &&
	grep " M" ../actual &&
	git checkout -- a.txt &&
	rm -f u.txt
	)
'

# === deeply nested untracked ===

test_expect_success 'deeply nested untracked file' '
	(
	cd repo &&
	mkdir -p deep/nested/path &&
	echo "deep" >deep/nested/path/file.txt &&
	git status --porcelain >../actual &&
	grep "deep/" ../actual &&
	rm -rf deep
	)
'

# === worktree modification then stage ===

test_expect_success 'staging worktree change moves it to index column' '
	(
	cd repo &&
	echo "staged now" >a.txt &&
	git add a.txt &&
	git status --porcelain >../actual &&
	grep "^M  a.txt" ../actual &&
	git reset HEAD a.txt &&
	git checkout -- a.txt
	)
'

# === additional edge cases ===

test_expect_success 'status after adding then resetting shows worktree mod' '
	(
	cd repo &&
	echo "temp change" >a.txt &&
	git add a.txt &&
	git reset HEAD a.txt &&
	git status --porcelain >../actual &&
	grep "^ M a.txt" ../actual &&
	git checkout -- a.txt
	)
'

test_expect_success 'status shows both worktree and index changes on different files' '
	(
	cd repo &&
	echo "staged" >a.txt &&
	git add a.txt &&
	echo "worktree" >b.txt &&
	git status --porcelain >../actual &&
	grep "^M  a.txt" ../actual &&
	grep "^ M b.txt" ../actual &&
	git reset HEAD a.txt &&
	git checkout -- .
	)
'

# === status after various operations ===

test_expect_success 'status clean after commit' '
	(
	cd repo &&
	echo "new content" >a.txt &&
	git add a.txt &&
	git commit -m "update a" &&
	git status -s >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'status clean after checkout restore' '
	(
	cd repo &&
	echo "temp" >a.txt &&
	git checkout -- a.txt &&
	git status -s >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'status with only whitespace change in file' '
	(
	cd repo &&
	printf "new content\n\n" >a.txt &&
	git status --porcelain >../actual &&
	grep "^ M a.txt" ../actual &&
	git checkout -- a.txt
	)
'

test_done
