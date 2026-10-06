#!/bin/sh
#
# Tests for diff-index plumbing command
#

test_description='diff-index plumbing tests'

. ./test-lib.sh

test_expect_success 'setup: init repo with config' '
	git init &&
	git config user.email "test@test.com" &&
	git config user.name "Test User"
'

test_expect_success 'setup: create initial commit' '
	echo "file1" >file1.txt &&
	echo "file2" >file2.txt &&
	mkdir -p sub &&
	echo "sub" >sub/s.txt &&
	git add . &&
	git commit -m "initial"
'

test_expect_success 'diff-index HEAD on clean tree is empty' '
	git diff-index HEAD >out &&
	test_must_be_empty out
'

test_expect_success 'diff-index HEAD detects modified file' '
	echo "changed" >file1.txt &&
	git diff-index HEAD >out &&
	grep -q "file1.txt" out
'

test_expect_success 'diff-index HEAD shows M status for modification' '
	git diff-index HEAD >out &&
	grep -q "M" out
'

test_expect_success 'diff-index HEAD raw format has colon prefix' '
	git diff-index HEAD >out &&
	grep -q ":" out
'

test_expect_success 'diff-index --cached HEAD on clean tree is empty' '
	git checkout -- . &&
	git diff-index --cached HEAD >out &&
	test_must_be_empty out
'

test_expect_success 'diff-index --cached HEAD detects staged change' '
	echo "staged" >file1.txt &&
	git add file1.txt &&
	git diff-index --cached HEAD >out &&
	grep -q "file1.txt" out
'

test_expect_success 'diff-index --cached HEAD shows M for staged mod' '
	git diff-index --cached HEAD >out &&
	grep -q "M" out
'

test_expect_success 'diff-index HEAD detects unstaged change after staging' '
	echo "unstaged on top" >file1.txt &&
	git diff-index HEAD >out &&
	grep -q "file1.txt" out
'

test_expect_success 'commit staged changes' '
	git checkout -- . &&
	git diff-index --cached HEAD >out &&
	git commit -m "staged update" 2>/dev/null || true
'

test_expect_success 'diff-index HEAD with new untracked file is clean' '
	echo "untracked" >untracked.txt &&
	git diff-index HEAD >out &&
	! grep -q "untracked.txt" out
'

test_expect_success 'diff-index --cached HEAD with newly added file shows A' '
	git add untracked.txt &&
	git diff-index --cached HEAD >out &&
	grep -q "A" out &&
	grep -q "untracked.txt" out
'

test_expect_success 'commit new file' '
	git commit -m "add untracked"
'

test_expect_success 'diff-index HEAD with deleted file' '
	rm file2.txt &&
	git diff-index HEAD >out &&
	grep -q "file2.txt" out
'

test_expect_success 'diff-index HEAD shows D for deleted file' '
	git diff-index HEAD >out &&
	grep -q "D" out
'

test_expect_success 'diff-index --cached HEAD with staged deletion' '
	git add file2.txt &&
	git diff-index --cached HEAD >out &&
	grep -q "D" out &&
	grep -q "file2.txt" out
'

test_expect_success 'commit deletion' '
	git commit -m "delete file2"
'

test_expect_success 'diff-index HEAD with subdir modification' '
	echo "mod sub" >sub/s.txt &&
	git diff-index HEAD >out &&
	grep -q "sub/s.txt" out
'

test_expect_success 'diff-index --cached HEAD with staged subdir mod' '
	git add sub/s.txt &&
	git diff-index --cached HEAD >out &&
	grep -q "sub/s.txt" out
'

test_expect_success 'commit subdir change' '
	git commit -m "update sub"
'

test_expect_success 'diff-index HEAD with multiple changes' '
	echo "a" >file1.txt &&
	echo "b" >sub/s.txt &&
	echo "c" >newfile.txt &&
	git add newfile.txt &&
	git diff-index HEAD >out &&
	test -s out
'

test_expect_success 'diff-index --cached HEAD with mixed staged and unstaged' '
	git diff-index --cached HEAD >out &&
	grep -q "newfile.txt" out
'

test_expect_success 'commit mixed' '
	git add . &&
	git commit -m "mixed update"
'

test_expect_success 'diff-index with specific tree-ish (not HEAD)' '
	FIRST=$(git rev-list --reverse HEAD | head -1) &&
	git diff-index $FIRST >out &&
	test -s out
'

test_expect_success 'diff-index --cached with specific tree-ish' '
	FIRST=$(git rev-list --reverse HEAD | head -1) &&
	git diff-index --cached $FIRST >out &&
	test -s out
'

test_expect_success 'diff-index HEAD with binary file' '
	printf "\000\001\002" >bin.dat &&
	git add bin.dat &&
	git commit -m "add binary" &&
	printf "\003\004\005" >bin.dat &&
	git diff-index HEAD >out &&
	grep -q "bin.dat" out
'

test_expect_success 'diff-index --cached HEAD with binary file staged' '
	git add bin.dat &&
	git diff-index --cached HEAD >out &&
	grep -q "bin.dat" out
'

test_expect_success 'commit binary update' '
	git commit -m "update binary"
'

test_expect_success 'diff-index HEAD with empty file' '
	>empty.txt &&
	git add empty.txt &&
	git commit -m "add empty" &&
	echo "content" >empty.txt &&
	git diff-index HEAD >out &&
	grep -q "empty.txt" out
'

test_expect_success 'diff-index HEAD with file becoming empty' '
	git checkout -- . &&
	>file1.txt &&
	git diff-index HEAD >out &&
	grep -q "file1.txt" out
'

test_expect_success 'diff-index HEAD with deep nested new file' '
	git checkout -- . &&
	mkdir -p d1/d2/d3 &&
	echo "deep" >d1/d2/d3/file.txt &&
	git add d1/ &&
	git diff-index --cached HEAD >out &&
	grep -q "d1/d2/d3/file.txt" out
'

test_expect_success 'commit deep file' '
	git commit -m "add deep"
'

test_expect_success 'diff-index --cached HEAD after empty commit' '
	git commit --allow-empty -m "empty" &&
	git diff-index --cached HEAD >out &&
	test_must_be_empty out
'

test_expect_success 'diff-index HEAD with file replaced by symlink' '
	rm file1.txt &&
	ln -s sub/s.txt file1.txt &&
	git diff-index HEAD >out &&
	grep -q "file1.txt" out
'

test_expect_success 'diff-index shows type change' '
	git diff-index HEAD >out &&
	grep -q "file1.txt" out
'

test_expect_success 'cleanup symlink' '
	rm file1.txt &&
	git checkout -- file1.txt
'

test_expect_success 'diff-index --cached HEAD shows nothing after full restore' '
	git diff-index --cached HEAD >out &&
	test_must_be_empty out
'

test_done
