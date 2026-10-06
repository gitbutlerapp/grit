#!/bin/sh

test_description='diff --cached (staged changes vs HEAD)'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
    grit init repo && cd repo &&
    git config user.email "t@t.com" && git config user.name "T" &&
    echo "hello" >file.txt &&
    echo "world" >other.txt &&
    grit add file.txt other.txt &&
    grit commit -m "initial"
	)
'

test_expect_success 'no cached diff with clean index' '
    (cd repo && grit diff --cached >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'cached diff after staging modification' '
    (cd repo && echo "hello-modified" >file.txt && grit add file.txt &&
     grit diff --cached >../actual) &&
    grep "^-hello$" actual &&
    grep "^+hello-modified$" actual
'

test_expect_success 'cached diff --name-only' '
    (cd repo && grit diff --cached --name-only >../actual) &&
    echo "file.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'cached diff --name-status shows M' '
    (cd repo && grit diff --cached --name-status >../actual) &&
    echo "M	file.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'cached diff --numstat' '
    (cd repo && grit diff --cached --numstat >../actual) &&
    echo "1	1	file.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'cached diff --stat' '
    (cd repo && grit diff --cached --stat >../actual) &&
    grep "file.txt" actual &&
    grep "1 file changed" actual
'

test_expect_success 'cached diff --exit-code returns 1 with changes' '
    (cd repo && test_must_fail grit diff --cached --exit-code)
'

test_expect_success 'cached diff -q suppresses output' '
    (cd repo && test_must_fail grit diff --cached -q >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'stage multiple files and diff --cached' '
    (cd repo && echo "world-mod" >other.txt && grit add other.txt &&
     grit diff --cached --name-only >../actual) &&
    printf "file.txt\nother.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'cached diff --name-status for multiple files' '
    (cd repo && grit diff --cached --name-status >../actual) &&
    grep "^M	file.txt$" actual &&
    grep "^M	other.txt$" actual
'

test_expect_success 'cached diff --numstat for multiple files' '
    (cd repo && grit diff --cached --numstat >../actual) &&
    grep "^1	1	file.txt$" actual &&
    grep "^1	1	other.txt$" actual
'

test_expect_success 'cached diff --stat summary for multiple files' '
    (cd repo && grit diff --cached --stat >../actual) &&
    grep "2 files changed" actual
'

test_expect_success 'commit and verify clean cached diff' '
    (cd repo && grit commit -m "modify both" &&
     grit diff --cached >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'cached diff --exit-code returns 0 with no changes' '
    (cd repo && grit diff --cached --exit-code)
'

test_expect_success 'staged new file shows as addition' '
    (cd repo && echo "brand new" >new.txt && grit add new.txt &&
     grit diff --cached --name-status >../actual) &&
    echo "A	new.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'staged new file diff shows content' '
    (cd repo && grit diff --cached >../actual) &&
    grep "^+brand new$" actual
'

test_expect_success 'staged new file --numstat' '
    (cd repo && grit diff --cached --numstat >../actual) &&
    echo "1	0	new.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'staged deletion shows D status' '
    (cd repo && grit commit -m "add new" &&
     grit rm other.txt &&
     grit diff --cached --name-status >../actual) &&
    echo "D	other.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'staged deletion --numstat' '
    (cd repo && grit diff --cached --numstat >../actual) &&
    echo "0	1	other.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'staged deletion diff shows removed content' '
    (cd repo && grit diff --cached >../actual) &&
    grep "^-world-mod$" actual
'

test_expect_success 'commit deletion and verify clean state' '
    (cd repo && grit commit -m "delete other" &&
     grit diff --cached --exit-code)
'

test_expect_success 'cached diff with nested directory additions' '
    (cd repo && mkdir -p sub/deep &&
     echo "nested" >sub/deep/nested.txt &&
     grit add sub/deep/nested.txt &&
     grit diff --cached --name-only >../actual) &&
    echo "sub/deep/nested.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'cached diff a/b prefix for nested paths' '
    (cd repo && grit diff --cached >../actual) &&
    grep "^--- /dev/null$" actual &&
    grep "^+++ b/sub/deep/nested.txt$" actual
'

test_expect_success 'multiple staged additions' '
    (cd repo && echo "x" >sub/x.txt && echo "y" >sub/y.txt &&
     grit add sub/x.txt sub/y.txt &&
     grit diff --cached --name-only >../actual) &&
    printf "sub/deep/nested.txt\nsub/x.txt\nsub/y.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'cached diff --stat with additions' '
    (cd repo && grit diff --cached --stat >../actual) &&
    grep "3 files changed" actual &&
    grep "insertion" actual
'

test_expect_success 'commit additions and stage mixed changes' '
    (cd repo && grit commit -m "add sub files" &&
     echo "modified-nested" >sub/deep/nested.txt &&
     grit rm sub/x.txt &&
     echo "new-z" >sub/z.txt &&
     grit add sub/deep/nested.txt sub/z.txt &&
     grit diff --cached --name-status >../actual) &&
    grep "^M	sub/deep/nested.txt$" actual &&
    grep "^D	sub/x.txt$" actual &&
    grep "^A	sub/z.txt$" actual
'

test_expect_success 'cached diff --numstat mixed changes' '
    (cd repo && grit diff --cached --numstat >../actual) &&
    grep "^1	1	sub/deep/nested.txt$" actual &&
    grep "^0	1	sub/x.txt$" actual &&
    grep "^1	0	sub/z.txt$" actual
'

test_expect_success 'cached diff --stat mixed changes summary' '
    (cd repo && grit diff --cached --stat >../actual) &&
    grep "3 files changed" actual
'

test_expect_success 'cached diff multiline file modification' '
    (cd repo && grit commit -m "mixed changes" &&
     printf "line1\nline2\nline3\nline4\nline5\n" >file.txt &&
     grit add file.txt && grit commit -m "multiline" &&
     printf "line1\nMODIFIED\nline3\nline4\nline5\n" >file.txt &&
     grit add file.txt &&
     grit diff --cached >../actual) &&
    grep "^-line2$" actual &&
    grep "^+MODIFIED$" actual
'

test_expect_success 'cached diff --exit-code with staged multiline change' '
    (cd repo && test_must_fail grit diff --cached --exit-code)
'

test_expect_success 'cached diff shows correct hunk header' '
    (cd repo && grit diff --cached >../actual) &&
    grep "^@@ " actual
'

test_expect_success 'staged empty file shows addition' '
    (cd repo && grit commit -m "save state" &&
     touch empty.txt && grit add empty.txt &&
     grit diff --cached --name-status >../actual) &&
    echo "A	empty.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'staged empty file diff is minimal' '
    (cd repo && grit diff --cached --numstat >../actual) &&
    echo "0	0	empty.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'staged diff with --staged alias' '
    (cd repo && grit diff --staged --name-only >../actual) &&
    echo "empty.txt" >expect &&
    test_cmp expect actual
'

test_done
