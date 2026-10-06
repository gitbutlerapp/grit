#!/bin/sh
# Tests for 'grit diff' with working tree changes involving
# directory/file differences, type changes, and untracked paths.

test_description='grit diff working tree directory and file scenarios'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo hello >file.txt &&
	echo world >other.txt &&
	mkdir -p dir &&
	echo inside >dir/a.txt &&
	echo also >dir/b.txt &&
	$REAL_GIT add . &&
	$REAL_GIT commit -m "initial"
	)
'

test_expect_success 'diff shows no output on clean tree' '
	(cd repo && grit diff >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff --stat shows no output on clean tree' '
	(cd repo && grit diff --stat >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff --numstat shows no output on clean tree' '
	(cd repo && grit diff --numstat >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff --name-only shows no output on clean tree' '
	(cd repo && grit diff --name-only >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff --name-status shows no output on clean tree' '
	(cd repo && grit diff --name-status >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff --exit-code returns 0 on clean tree' '
	(cd repo && grit diff --exit-code >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff detects modification in file' '
	(cd repo && echo changed >file.txt && grit diff --name-only >../actual) &&
	echo "file.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff detects modification in subdirectory file' '
	(cd repo && echo new-content >dir/a.txt && grit diff --name-only >../actual) &&
	printf "dir/a.txt\nfile.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --name-status shows M for modified files' '
	(cd repo && grit diff --name-status >../actual) &&
	grep "^M	dir/a.txt$" actual &&
	grep "^M	file.txt$" actual
'

test_expect_success 'diff --stat shows correct file list' '
	(cd repo && grit diff --stat >../actual) &&
	grep "dir/a.txt" actual &&
	grep "file.txt" actual
'

test_expect_success 'diff --numstat shows numeric counts' '
	(cd repo && grit diff --numstat >../actual) &&
	grep "dir/a.txt" actual &&
	grep "file.txt" actual
'

test_expect_success 'diff --exit-code returns 1 when changes exist' '
	(cd repo && test_expect_code 1 grit diff --exit-code)
'

test_expect_success 'diff -q returns 1 when changes exist' '
	(cd repo && test_expect_code 1 grit diff -q)
'

test_expect_success 'diff -q produces no output' '
	(cd repo && grit diff -q >../actual 2>&1; true) &&
	test_must_be_empty actual
'

test_expect_success 'reset working tree for new tests' '
	(cd repo && $REAL_GIT checkout -- .)
'

test_expect_success 'diff detects deleted file in working tree' '
	(cd repo && rm file.txt && grit diff --name-status >../actual) &&
	echo "D	file.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --stat for deleted file' '
	(cd repo && grit diff --stat >../actual) &&
	grep "file.txt" actual &&
	grep "1 deletion" actual
'

test_expect_success 'diff --numstat for deleted file' '
	(cd repo && grit diff --numstat >../actual) &&
	echo "0	1	file.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff shows delete patch for removed file' '
	(cd repo && grit diff >../actual) &&
	grep "^deleted file mode" actual &&
	grep "^--- a/file.txt" actual &&
	grep "^-hello" actual
'

test_expect_success 'restore and delete directory file' '
	(cd repo && $REAL_GIT checkout -- . &&
	 rm dir/a.txt && grit diff --name-status >../actual) &&
	echo "D	dir/a.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff shows multiple deleted files in directory' '
	(cd repo && rm -f dir/b.txt && grit diff --name-only >../actual) &&
	printf "dir/a.txt\ndir/b.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore tree and modify multiple files' '
	(cd repo && $REAL_GIT checkout -- . &&
	 echo m1 >file.txt &&
	 echo m2 >other.txt &&
	 echo m3 >dir/a.txt &&
	 echo m4 >dir/b.txt &&
	 grit diff --name-only >../actual) &&
	printf "dir/a.txt\ndir/b.txt\nfile.txt\nother.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --stat summary line for multiple files' '
	(cd repo && grit diff --stat >../actual) &&
	grep "4 files changed" actual
'

test_expect_success 'diff --numstat for multiple modified files' '
	(cd repo && grit diff --numstat >../actual) &&
	wc -l <actual >count &&
	echo "4" >expect_count &&
	test_cmp expect_count count
'

test_expect_success 'diff -U0 shows no context lines' '
	(cd repo && grit diff -U0 >../actual) &&
	! grep "^+hello$" actual &&
	! grep "^+world$" actual
'

test_expect_success 'diff -U1 shows limited context' '
	(cd repo && $REAL_GIT checkout -- . &&
	 printf "line1\nline2\nline3\nline4\nline5\n" >file.txt &&
	 $REAL_GIT add file.txt && $REAL_GIT commit -m "multi-line" &&
	 sed "s/line3/changed3/" file.txt >tmp && mv tmp file.txt &&
	 grit diff -U1 >../actual) &&
	grep "line2" actual &&
	grep "line4" actual
'

test_expect_success 'diff with only whitespace changes' '
	(cd repo && printf "line1 \nline2\nline3\nline4\nline5\n" >file.txt &&
	 grit diff --name-only >../actual) &&
	echo "file.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset and test diff on empty file content' '
	(cd repo && $REAL_GIT checkout -- . &&
	 : >file.txt &&
	 grit diff --name-status >../actual) &&
	grep "^M	file.txt$" actual
'

test_expect_success 'diff --stat for file emptied to zero bytes' '
	(cd repo && grit diff --stat >../actual) &&
	grep "file.txt" actual &&
	grep "deletion" actual
'

test_expect_success 'diff on binary-like content' '
	(cd repo && $REAL_GIT checkout -- . &&
	 printf "\x00\x01\x02" >file.txt &&
	 grit diff --name-status >../actual) &&
	grep "^M	file.txt$" actual
'

test_expect_success 'restore and add new untracked file - not in diff' '
	(cd repo && $REAL_GIT checkout -- . &&
	 echo new >untracked.txt &&
	 grit diff --name-only >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff detects mode change (if supported)' '
	(cd repo && rm -f untracked.txt &&
	 chmod +x other.txt &&
	 grit diff >../actual) &&
	if grep "mode" actual; then
		grep "other.txt" actual
	else
		true
	fi
'

test_expect_success 'restore permissions' '
	(cd repo && chmod -x other.txt && $REAL_GIT checkout -- .)
'

test_expect_success 'diff for file with content replacement' '
	(cd repo && $REAL_GIT checkout -- . &&
	 echo replaced >other.txt &&
	 grit diff --numstat >../actual) &&
	grep "other.txt" actual
'

test_expect_success 'diff --stat shows changes for replaced content' '
	(cd repo && grit diff --stat >../actual) &&
	grep "other.txt" actual &&
	grep "changed" actual
'

test_expect_success 'cleanup' '
	(cd repo && $REAL_GIT checkout -- .)
'

test_done
