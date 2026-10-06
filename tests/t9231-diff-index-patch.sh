#!/bin/sh

test_description='diff-index -p (patch output)'

. ./test-lib.sh

test_expect_success 'setup' '
	git init &&
	git config user.email test@test.com &&
	git config user.name "Test User" &&
	echo "line1" >file.txt &&
	echo "line2" >>file.txt &&
	echo "line3" >>file.txt &&
	git add file.txt &&
	test_tick &&
	git commit -m "initial"
'

test_expect_success 'diff-index -p shows unified diff for staged changes' '
	echo "modified" >>file.txt &&
	git add file.txt &&
	git diff-index -p --cached HEAD >actual &&
	grep "^diff --git" actual &&
	grep "^---" actual &&
	grep "^+++" actual &&
	grep "^@@" actual &&
	grep "+modified" actual
'

test_expect_success 'diff-index -p shows diff for worktree changes' '
	echo "unstaged" >>file.txt &&
	git diff-index -p HEAD >actual &&
	grep "^diff --git" actual &&
	grep "+unstaged" actual
'

test_expect_success 'diff-index --patch is alias for -p' '
	git diff-index --patch HEAD >actual &&
	grep "^diff --git" actual
'

test_expect_success 'diff-index -p with -U0 shows zero context' '
	git diff-index -p -U0 --cached HEAD >actual &&
	grep "^@@" actual
'

test_expect_success 'diff-index --stat shows stat output' '
	git diff-index --stat --cached HEAD >actual &&
	grep "file.txt" actual &&
	grep "changed" actual
'

test_expect_success 'diff-index --numstat shows numeric stat' '
	git diff-index --numstat --cached HEAD >actual &&
	grep "file.txt" actual
'

test_expect_success 'diff-index --name-only shows just filenames' '
	git diff-index --name-only --cached HEAD >actual &&
	grep "^file.txt$" actual
'

test_expect_success 'diff-index --name-status shows status and filename' '
	git diff-index --name-status --cached HEAD >actual &&
	grep "M" actual &&
	grep "file.txt" actual
'

test_expect_success 'diff-index -p for added file' '
	echo "new content" >new.txt &&
	git add new.txt &&
	git diff-index -p --cached HEAD >actual &&
	grep "^diff --git a/new.txt b/new.txt" actual &&
	grep "new file mode" actual &&
	grep "+new content" actual
'

test_expect_success 'diff-index -p for deleted file' '
	git rm -f file.txt &&
	git diff-index -p --cached HEAD >actual &&
	grep "deleted file mode" actual &&
	grep "^-line1" actual
'

test_done
