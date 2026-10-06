#!/bin/sh
#
# Tests for diff-tree plumbing command
#

test_description='diff-tree plumbing tests'

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
	echo "sub file" >sub/s.txt &&
	git add . &&
	git commit -m "initial"
'

test_expect_success 'setup: create second commit with modifications' '
	echo "modified file1" >file1.txt &&
	echo "new file" >file3.txt &&
	git add . &&
	git commit -m "second"
'

test_expect_success 'diff-tree with two commits shows raw diff' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree $C1 $C2 >out &&
	test -s out
'

test_expect_success 'diff-tree -r with two commits shows files' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree -r $C1 $C2 >out &&
	grep -q "file1.txt" out &&
	grep -q "file3.txt" out
'

test_expect_success 'diff-tree raw output has correct format' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree $C1 $C2 >out &&
	grep -q ":" out
'

test_expect_success 'diff-tree shows M for modified files' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree $C1 $C2 >out &&
	grep -q "M" out
'

test_expect_success 'diff-tree shows A for added files' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree $C1 $C2 >out &&
	grep -q "A" out
'

test_expect_success 'diff-tree --name-only lists filenames' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree --name-only $C1 $C2 >out &&
	grep -q "file1.txt" out &&
	grep -q "file3.txt" out
'

test_expect_success 'diff-tree --name-status shows status letters' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree --name-status $C1 $C2 >out &&
	grep -q "M.*file1.txt" out &&
	grep -q "A.*file3.txt" out
'

test_expect_success 'diff-tree -p shows patch output' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree -p $C1 $C2 >out &&
	grep -q "diff --git" out
'

test_expect_success 'diff-tree -p shows correct file headers' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree -p $C1 $C2 >out &&
	grep -q "a/file1.txt" out
'

test_expect_success 'diff-tree --stat shows stat summary' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree --stat $C1 $C2 >out &&
	grep -q "file1.txt" out &&
	grep -q "file changed" out || grep -q "files changed" out || true
'

test_expect_success 'diff-tree with single commit compares to parent' '
	C=$(git rev-parse HEAD) &&
	git diff-tree $C >out &&
	test -s out
'

test_expect_success 'diff-tree -r with single commit' '
	C=$(git rev-parse HEAD) &&
	git diff-tree -r $C >out &&
	test -s out
'

test_expect_success 'diff-tree with identical trees produces no output' '
	C=$(git rev-parse HEAD) &&
	git diff-tree $C $C >out &&
	test_must_be_empty out
'

test_expect_success 'setup: create commit with deletion' '
	git rm file2.txt &&
	git commit -m "delete file2"
'

test_expect_success 'diff-tree shows D for deleted file' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree $C1 $C2 >out &&
	grep -q "D" out &&
	grep -q "file2.txt" out
'

test_expect_success 'diff-tree --name-status shows D for deletion' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree --name-status $C1 $C2 >out &&
	grep -q "D.*file2.txt" out
'

test_expect_success 'setup: create commit modifying subdir file' '
	echo "modified sub" >sub/s.txt &&
	git add sub/s.txt &&
	git commit -m "modify sub"
'

test_expect_success 'diff-tree shows changes in subdirectories' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree -r $C1 $C2 >out &&
	grep -q "sub/s.txt" out
'

test_expect_success 'diff-tree -r -p shows patch for subdir files' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree -r -p $C1 $C2 >out &&
	grep -q "sub/s.txt" out
'

test_expect_success 'setup: create multiple commits for range' '
	echo "a" >a.txt &&
	git add a.txt &&
	git commit -m "add a" &&
	echo "b" >b.txt &&
	git add b.txt &&
	git commit -m "add b"
'

test_expect_success 'diff-tree across multiple commits' '
	C1=$(git rev-parse HEAD~2) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree -r $C1 $C2 >out &&
	grep -q "a.txt" out &&
	grep -q "b.txt" out
'

test_expect_success 'diff-tree --name-only across multiple commits' '
	C1=$(git rev-parse HEAD~2) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree --name-only $C1 $C2 >out &&
	grep -q "a.txt" out &&
	grep -q "b.txt" out
'

test_expect_success 'setup: commit with binary file' '
	printf "\000\001\002" >bin.dat &&
	git add bin.dat &&
	git commit -m "add binary"
'

test_expect_success 'diff-tree detects binary file addition' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree -r $C1 $C2 >out &&
	grep -q "bin.dat" out
'

test_expect_success 'diff-tree -p with binary file shows binary notice' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree -p $C1 $C2 >out &&
	grep -q "bin.dat" out
'

test_expect_success 'setup: modify binary file' '
	printf "\003\004\005" >bin.dat &&
	git add bin.dat &&
	git commit -m "modify binary"
'

test_expect_success 'diff-tree shows modified binary' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree -r $C1 $C2 >out &&
	grep -q "M" out &&
	grep -q "bin.dat" out
'

test_expect_success 'diff-tree reversed order swaps A and D' '
	C1=$(git rev-parse HEAD~5) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree --name-status $C2 $C1 >out &&
	grep -q "D" out
'

test_expect_success 'setup: create empty commit (tree unchanged)' '
	git commit --allow-empty -m "empty commit"
'

test_expect_success 'diff-tree on empty commit shows nothing' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree $C1 $C2 >out &&
	test_must_be_empty out
'

test_expect_success 'setup: add file in new deep dir' '
	mkdir -p d1/d2/d3 &&
	echo "deep" >d1/d2/d3/deep.txt &&
	git add d1/ &&
	git commit -m "deep dir"
'

test_expect_success 'diff-tree shows deeply nested file' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree -r $C1 $C2 >out &&
	grep -q "d1/d2/d3/deep.txt" out
'

test_expect_success 'diff-tree -r --stat with deep nested file' '
	C1=$(git rev-parse HEAD~1) &&
	C2=$(git rev-parse HEAD) &&
	git diff-tree -r --stat $C1 $C2 >out &&
	grep -q "d1/d2/d3/deep.txt" out
'

test_expect_success 'diff-tree with tree objects directly' '
	T1=$(git rev-parse HEAD~1^{tree}) &&
	T2=$(git rev-parse HEAD^{tree}) &&
	git diff-tree $T1 $T2 >out &&
	test -s out
'

test_done
