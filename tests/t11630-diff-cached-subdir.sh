#!/bin/sh
#
# Tests for diff --cached with files in subdirectories
#

test_description='diff --cached with subdirectories'

. ./test-lib.sh

test_expect_success 'setup: init repo with config' '
	git init &&
	git config user.email "test@test.com" &&
	git config user.name "Test User"
'

test_expect_success 'setup: create directory structure and commit' '
	mkdir -p sub/deep &&
	echo "root file" >root.txt &&
	echo "sub file" >sub/file.txt &&
	echo "deep file" >sub/deep/file.txt &&
	git add . &&
	git commit -m "initial"
'

test_expect_success 'diff --cached empty after clean commit' '
	git diff --cached >out &&
	test_must_be_empty out
'

test_expect_success 'diff --cached detects staged change in root' '
	echo "modified root" >root.txt &&
	git add root.txt &&
	git diff --cached >out &&
	test -s out &&
	grep -q "root.txt" out
'

test_expect_success 'diff --cached detects staged change in subdir' '
	git commit -m "update root" &&
	echo "modified sub" >sub/file.txt &&
	git add sub/file.txt &&
	git diff --cached >out &&
	test -s out &&
	grep -q "sub/file.txt" out
'

test_expect_success 'diff --cached detects staged change in deep subdir' '
	git commit -m "update sub" &&
	echo "modified deep" >sub/deep/file.txt &&
	git add sub/deep/file.txt &&
	git diff --cached >out &&
	test -s out &&
	grep -q "sub/deep/file.txt" out
'

test_expect_success 'diff --cached --name-only shows correct paths' '
	git diff --cached --name-only >out &&
	grep -q "sub/deep/file.txt" out
'

test_expect_success 'diff --cached --name-status shows M for modified' '
	git diff --cached --name-status >out &&
	grep -q "M" out
'

test_expect_success 'diff --cached --stat shows subdir file' '
	git diff --cached --stat >out &&
	grep -q "sub/deep/file.txt" out
'

test_expect_success 'diff --cached with multiple staged files across dirs' '
	git commit -m "update deep" &&
	echo "change1" >root.txt &&
	echo "change2" >sub/file.txt &&
	echo "change3" >sub/deep/file.txt &&
	git add . &&
	git diff --cached --name-only >out &&
	grep -q "root.txt" out &&
	grep -q "sub/file.txt" out &&
	grep -q "sub/deep/file.txt" out
'

test_expect_success 'diff --cached --numstat shows stats for subdirs' '
	git diff --cached --numstat >out &&
	test -s out
'

test_expect_success 'commit and verify clean state' '
	git commit -m "update all" &&
	git diff --cached >out &&
	test_must_be_empty out
'

test_expect_success 'diff --cached with new file in subdir' '
	echo "brand new" >sub/new.txt &&
	git add sub/new.txt &&
	git diff --cached --name-status >out &&
	grep -q "A" out &&
	grep -q "sub/new.txt" out
'

test_expect_success 'diff --cached with new file in deep subdir' '
	echo "deep new" >sub/deep/new.txt &&
	git add sub/deep/new.txt &&
	git diff --cached --name-only >out &&
	grep -q "sub/deep/new.txt" out
'

test_expect_success 'diff --cached with deleted file in subdir' '
	git commit -m "add new files" &&
	git rm sub/new.txt &&
	git diff --cached --name-status >out &&
	grep -q "D" out &&
	grep -q "sub/new.txt" out
'

test_expect_success 'diff --cached with deleted file in deep subdir' '
	git rm sub/deep/new.txt &&
	git diff --cached --name-only >out &&
	grep -q "sub/deep/new.txt" out
'

test_expect_success 'commit deletions' '
	git commit -m "delete new files"
'

test_expect_success 'diff --cached with new directory' '
	mkdir -p another/level &&
	echo "content" >another/level/file.txt &&
	git add another/ &&
	git diff --cached --name-only >out &&
	grep -q "another/level/file.txt" out
'

test_expect_success 'diff --cached shows correct status for new dir files' '
	git diff --cached --name-status >out &&
	grep -q "A" out
'

test_expect_success 'commit new directory and verify clean' '
	git commit -m "add another dir" &&
	git diff --cached >out &&
	test_must_be_empty out
'

test_expect_success 'diff --cached with renamed file in subdir (via rm+add)' '
	git rm sub/file.txt &&
	echo "modified sub" >sub/renamed.txt &&
	git add sub/renamed.txt &&
	git diff --cached --name-status >out &&
	test -s out
'

test_expect_success 'commit rename and verify' '
	git commit -m "rename in sub"
'

test_expect_success 'diff --cached --stat with mixed operations' '
	echo "new root" >root2.txt &&
	echo "mod deep" >sub/deep/file.txt &&
	git add . &&
	git diff --cached --stat >out &&
	test -s out
'

test_expect_success 'diff --cached exit code is 0 with staged changes' '
	test_must_fail git diff --cached --exit-code
'

test_expect_success 'commit and verify exit-code 0 on clean' '
	git commit -m "mixed changes" &&
	git diff --cached --exit-code
'

test_expect_success 'diff --cached --quiet with no staged changes exits 0' '
	git diff --cached --quiet
'

test_expect_success 'diff --cached --quiet with staged changes exits 1' '
	echo "quiet test" >root.txt &&
	git add root.txt &&
	test_must_fail git diff --cached --quiet
'

test_expect_success 'diff --cached with binary file in subdir' '
	git commit -m "quiet" &&
	printf "\000\001" >sub/bin.dat &&
	git add sub/bin.dat &&
	git diff --cached >out &&
	test -s out
'

test_expect_success 'diff --cached with empty file in subdir' '
	git commit -m "add binary" &&
	>sub/empty.txt &&
	git add sub/empty.txt &&
	git diff --cached --name-only >out &&
	grep -q "sub/empty.txt" out
'

test_expect_success 'diff --cached -U0 with subdir file' '
	git commit -m "add empty" &&
	echo "ctx test" >sub/deep/file.txt &&
	git add sub/deep/file.txt &&
	git diff --cached -U0 >out &&
	test -s out
'

test_expect_success 'diff --cached -U5 with subdir file' '
	git diff --cached -U5 >out &&
	test -s out
'

test_expect_success 'commit final changes' '
	git commit -m "context tests"
'

test_expect_success 'diff --cached with three-level deep new file' '
	mkdir -p a/b/c &&
	echo "deep" >a/b/c/file.txt &&
	git add a/ &&
	git diff --cached --name-only >out &&
	grep -q "a/b/c/file.txt" out
'

test_expect_success 'diff --cached with multiple new dirs' '
	mkdir -p x/y &&
	echo "x" >x/y/f.txt &&
	git add x/ &&
	git diff --cached --name-only >out &&
	grep -q "a/b/c/file.txt" out &&
	grep -q "x/y/f.txt" out
'

test_expect_success 'final commit and clean state' '
	git commit -m "deep dirs" &&
	git diff --cached >out &&
	test_must_be_empty out
'

test_done
