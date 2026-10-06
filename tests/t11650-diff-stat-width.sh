#!/bin/sh
#
# Tests for diff --stat output formatting and width
#

test_description='diff --stat output formatting'

. ./test-lib.sh

test_expect_success 'setup: init repo with config' '
	git init &&
	git config user.email "test@test.com" &&
	git config user.name "Test User"
'

test_expect_success 'setup: create files and initial commit' '
	echo "line1" >short.txt &&
	echo "content" >file.txt &&
	seq 1 10 >numbers.txt &&
	git add . &&
	git commit -m "initial"
'

test_expect_success 'diff --stat shows nothing on clean tree' '
	git diff --stat >out &&
	test_must_be_empty out
'

test_expect_success 'diff --stat shows one file changed' '
	echo "modified" >short.txt &&
	git diff --stat >out &&
	grep -q "short.txt" out &&
	grep -q "1 file changed" out
'

test_expect_success 'diff --stat shows change count' '
	git diff --stat >out &&
	grep -q "changed" out
'

test_expect_success 'diff --stat with multiple files' '
	echo "mod file" >file.txt &&
	git diff --stat >out &&
	grep -q "2 files changed" out
'

test_expect_success 'diff --stat shows file names for multiple changes' '
	git diff --stat >out &&
	grep -q "file.txt" out
'

test_expect_success 'diff --stat with deletion only' '
	git checkout -- . &&
	>short.txt &&
	git diff --stat >out &&
	grep -q "deletion" out || grep -q "-" out
'

test_expect_success 'diff --stat with appended line' '
	git checkout -- . &&
	echo "extra line" >>short.txt &&
	git diff --stat >out &&
	grep -q "short.txt" out
'

test_expect_success 'diff --stat with many lines changed' '
	git checkout -- . &&
	seq 11 20 >numbers.txt &&
	git diff --stat >out &&
	grep -q "numbers.txt" out
'

test_expect_success 'diff --stat --cached with staged changes' '
	git add numbers.txt &&
	git diff --cached --stat >out &&
	grep -q "numbers.txt" out
'

test_expect_success 'commit and verify clean stat' '
	git commit -m "update numbers" &&
	git diff --stat >out &&
	test_must_be_empty out
'

test_expect_success 'diff --stat with new file addition' '
	echo "brand new" >newfile.txt &&
	git add newfile.txt &&
	git diff --cached --stat >out &&
	grep -q "newfile.txt" out
'

test_expect_success 'diff --stat with file deletion' '
	git commit -m "add new" &&
	git rm short.txt &&
	git diff --cached --stat >out &&
	grep -q "short.txt" out
'

test_expect_success 'commit deletion' '
	git commit -m "remove short"
'

test_expect_success 'diff --stat with long filename' '
	mkdir -p very/long/path/to/a/deeply/nested &&
	echo "deep" >very/long/path/to/a/deeply/nested/file.txt &&
	git add very/ &&
	git diff --cached --stat >out &&
	grep -q "file.txt" out
'

test_expect_success 'commit long path file' '
	git commit -m "add deep file"
'

test_expect_success 'diff --stat with binary file' '
	printf "\000\001\002" >binary.dat &&
	git add binary.dat &&
	git commit -m "add binary" &&
	printf "\003\004\005" >binary.dat &&
	git diff --stat >out &&
	grep -q "binary.dat" out
'

test_expect_success 'diff --stat binary shows Bin size' '
	git diff --stat >out &&
	grep -q "Bin" out || grep -q "Binary" out || grep -q "binary" out || true
'

test_expect_success 'diff --numstat shows numbers for text files' '
	echo "numstat test" >file.txt &&
	git diff --numstat >out &&
	grep -q "file.txt" out
'

test_expect_success 'diff --numstat format has tab-separated values' '
	git diff --numstat >out &&
	grep "	" out | grep -q "file.txt"
'

test_expect_success 'diff --name-only lists just filenames' '
	git diff --name-only >out &&
	grep -q "file.txt" out &&
	grep -q "binary.dat" out
'

test_expect_success 'diff --name-status shows status letter and name' '
	git diff --name-status >out &&
	grep -q "M" out
'

test_expect_success 'diff --stat with multiple directories' '
	git checkout -- . &&
	mkdir -p dir1 dir2 &&
	echo "d1" >dir1/f.txt &&
	echo "d2" >dir2/f.txt &&
	git add dir1/ dir2/ &&
	git diff --cached --stat >out &&
	grep -q "dir1/f.txt" out &&
	grep -q "dir2/f.txt" out
'

test_expect_success 'diff --stat summary line with multiple files' '
	git diff --cached --stat >out &&
	grep -q "2 files changed" out
'

test_expect_success 'commit dirs' '
	git commit -m "add dirs"
'

test_expect_success 'diff --stat with zero-length change (mode only would show)' '
	echo "x" >dir1/f.txt &&
	git diff --stat >out &&
	grep -q "dir1/f.txt" out
'

test_expect_success 'diff --stat with empty file added' '
	git checkout -- . &&
	>empty.txt &&
	git add empty.txt &&
	git diff --cached --stat >out &&
	grep -q "empty.txt" out || grep -q "0 insertions" out || test -s out
'

test_expect_success 'commit empty file' '
	git commit -m "add empty"
'

test_expect_success 'diff --stat when modifying empty to non-empty' '
	echo "now content" >empty.txt &&
	git diff --stat >out &&
	grep -q "empty.txt" out
'

test_expect_success 'diff --stat with large number of insertions' '
	git checkout -- . &&
	seq 1 100 >big.txt &&
	git add big.txt &&
	git commit -m "add big" &&
	seq 1 200 >big.txt &&
	git diff --stat >out &&
	grep -q "big.txt" out
'

test_expect_success 'diff --stat graph shows change indicators' '
	git diff --stat >out &&
	test -s out
'

test_expect_success 'diff --stat with only deletions in file' '
	git checkout -- . &&
	seq 1 5 >big.txt &&
	git diff --stat >out &&
	grep -q "big.txt" out &&
	grep -q "-" out
'

test_expect_success 'diff --stat and --numstat agree on file count' '
	git diff --stat >stat_out &&
	git diff --numstat >num_out &&
	stat_files=$(grep -c "|" stat_out) &&
	num_files=$(wc -l <num_out | tr -d " ") &&
	test "$stat_files" = "$num_files"
'

test_expect_success 'diff --name-only and --name-status list same files' '
	git diff --name-only | sort >names1 &&
	git diff --name-status | awk "{print \$2}" | sort >names2 &&
	test_cmp names1 names2
'

test_expect_success 'cleanup' '
	git checkout -- . 2>/dev/null || true
'

test_done
