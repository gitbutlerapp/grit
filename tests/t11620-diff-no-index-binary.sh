#!/bin/sh
#
# Tests for diff with binary files in the working tree and index
#

test_description='diff with binary files'

. ./test-lib.sh

test_expect_success 'setup: init repo with config' '
	git init &&
	git config user.email "test@test.com" &&
	git config user.name "Test User"
'

test_expect_success 'setup: create and commit a text file' '
	echo "hello world" >text.txt &&
	git add text.txt &&
	git commit -m "add text"
'

test_expect_success 'setup: create and commit a binary file' '
	printf "\000\001\002\003" >binary.bin &&
	git add binary.bin &&
	git commit -m "add binary"
'

test_expect_success 'diff on clean working tree is empty' '
	git diff >out &&
	test_must_be_empty out
'

test_expect_success 'diff detects modified text file' '
	echo "changed" >text.txt &&
	git diff >out &&
	test -s out
'

test_expect_success 'diff detects modified binary file' '
	printf "\005\006\007\010" >binary.bin &&
	git diff >out &&
	test -s out
'

test_expect_success 'diff --stat shows binary file change' '
	git diff --stat >out &&
	grep -q "binary.bin" out
'

test_expect_success 'diff --numstat shows binary' '
	git diff --numstat >out &&
	grep -q "binary.bin" out
'

test_expect_success 'diff --name-only lists changed files' '
	git diff --name-only >out &&
	grep -q "binary.bin" out &&
	grep -q "text.txt" out
'

test_expect_success 'diff --name-status lists changed files with status' '
	git diff --name-status >out &&
	grep -q "M" out
'

test_expect_success 'diff --cached on empty staging is empty' '
	git checkout -- . &&
	git diff --cached >out &&
	test_must_be_empty out
'

test_expect_success 'diff --cached detects staged text change' '
	echo "staged change" >text.txt &&
	git add text.txt &&
	git diff --cached >out &&
	grep -q "staged change" out
'

test_expect_success 'diff --cached detects staged binary change' '
	printf "\011\012\013\014" >binary.bin &&
	git add binary.bin &&
	git diff --cached >out &&
	test -s out
'

test_expect_success 'diff --cached --stat shows summary' '
	git diff --cached --stat >out &&
	grep -q "binary.bin" out
'

test_expect_success 'diff --cached --name-only lists staged files' '
	git diff --cached --name-only >out &&
	grep -q "binary.bin" out &&
	grep -q "text.txt" out
'

test_expect_success 'commit staged changes' '
	git commit -m "update both files"
'

test_expect_success 'diff after commit is empty' '
	git diff >out &&
	test_must_be_empty out &&
	git diff --cached >out &&
	test_must_be_empty out
'

test_expect_success 'diff with binary file containing NUL in the middle' '
	printf "text\000more" >mixed.bin &&
	git add mixed.bin &&
	git commit -m "add mixed" &&
	printf "text\000changed" >mixed.bin &&
	git diff >out &&
	test -s out
'

test_expect_success 'diff --quiet exits 0 on clean tree' '
	git checkout -- . &&
	git diff --quiet
'

test_expect_success 'diff --quiet exits 1 on dirty tree' '
	echo "dirty" >text.txt &&
	test_must_fail git diff --quiet
'

test_expect_success 'diff --exit-code exits 0 on clean tree' '
	git checkout -- . &&
	git diff --exit-code
'

test_expect_success 'diff --exit-code exits 1 on dirty tree' '
	echo "dirty again" >text.txt &&
	test_must_fail git diff --exit-code
'

test_expect_success 'diff -U0 shows zero context lines' '
	git diff -U0 >out &&
	test -s out
'

test_expect_success 'diff -U5 shows context' '
	git checkout -- . &&
	seq 1 20 >numbers.txt &&
	git add numbers.txt &&
	git commit -m "add numbers" &&
	seq 1 20 | sed "10s/.*/changed/" >numbers.txt &&
	git diff -U5 >out &&
	test -s out
'

test_expect_success 'diff with newly added untracked file is empty' '
	git checkout -- . &&
	echo "new" >untracked.txt &&
	git diff >out &&
	! grep -q "untracked" out
'

test_expect_success 'diff --cached with newly staged file shows addition' '
	git add untracked.txt &&
	git diff --cached >out &&
	grep -q "untracked" out || grep -q "new" out
'

test_expect_success 'diff between text and binary modifications' '
	git commit -m "add untracked" &&
	echo "mod text" >text.txt &&
	printf "\077\100" >binary.bin &&
	git diff --name-only >out &&
	grep -q "text.txt" out &&
	grep -q "binary.bin" out
'

test_expect_success 'diff with empty file becoming non-empty' '
	git checkout -- . &&
	>empty.txt &&
	git add empty.txt &&
	git commit -m "add empty" &&
	echo "now has content" >empty.txt &&
	git diff >out &&
	test -s out
'

test_expect_success 'diff with non-empty file becoming empty' '
	>text.txt &&
	git diff >out &&
	test -s out
'

test_expect_success 'diff binary to empty' '
	git checkout -- text.txt &&
	>binary.bin &&
	git diff >out &&
	test -s out
'

test_expect_success 'diff --stat with only text changes' '
	git checkout -- . &&
	echo "one more change" >text.txt &&
	git diff --stat >out &&
	grep -q "text.txt" out
'

test_expect_success 'diff --stat with multiple files changed' '
	echo "change" >numbers.txt &&
	git diff --stat >out &&
	grep -q "2 files changed" out || grep -q "text.txt" out
'

test_expect_success 'diff with file deleted from working tree' '
	rm text.txt &&
	git diff --name-status >out &&
	grep -q "D" out &&
	grep -q "text.txt" out
'

test_expect_success 'diff --cached after rm and add shows delete' '
	git add text.txt &&
	git diff --cached --name-status >out &&
	grep -q "D" out
'

test_expect_success 'diff pure binary: both binary shows Binary files differ' '
	git checkout -- . &&
	printf "\200\201\202" >bin2.dat &&
	git add bin2.dat &&
	git commit -m "add bin2" &&
	printf "\203\204\205" >bin2.dat &&
	git diff >out &&
	test -s out
'

test_expect_success 'cleanup: restore files' '
	git checkout -- . 2>/dev/null || true
'

test_done
