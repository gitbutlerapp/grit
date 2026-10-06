#!/bin/sh

test_description='diff across nested directory structures (recursive tree diff behavior)'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with nested dirs' '
	(
    grit init repo && cd repo &&
    git config user.email "t@t.com" && git config user.name "T" &&
    mkdir -p dir1/sub1 dir2/sub2 &&
    echo "root" >root.txt &&
    echo "a" >dir1/a.txt &&
    echo "b" >dir1/sub1/b.txt &&
    echo "c" >dir2/c.txt &&
    echo "d" >dir2/sub2/d.txt &&
    grit add . &&
    grit commit -m "initial"
	)
'

test_expect_success 'no diff when working tree matches HEAD' '
    (cd repo && grit diff >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'diff detects change in root file' '
    (cd repo && echo "root-mod" >root.txt && grit diff --name-only >../actual) &&
    echo "root.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'diff detects change in nested dir file' '
    (cd repo && echo "a-mod" >dir1/a.txt && grit diff --name-only >../actual) &&
    printf "dir1/a.txt\nroot.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'diff detects change in deeply nested file' '
    (cd repo && echo "b-mod" >dir1/sub1/b.txt && grit diff --name-only >../actual) &&
    printf "dir1/a.txt\ndir1/sub1/b.txt\nroot.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'diff detects changes across multiple nested dirs' '
    (cd repo && echo "c-mod" >dir2/c.txt && grit diff --name-only >../actual) &&
    printf "dir1/a.txt\ndir1/sub1/b.txt\ndir2/c.txt\nroot.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'diff --name-status shows M for modifications' '
    (cd repo && grit diff --name-status >../actual) &&
    grep "^M	dir1/a.txt$" actual &&
    grep "^M	dir1/sub1/b.txt$" actual &&
    grep "^M	dir2/c.txt$" actual &&
    grep "^M	root.txt$" actual
'

test_expect_success 'diff --stat lists all modified files' '
    (cd repo && grit diff --stat >../actual) &&
    grep "dir1/a.txt" actual &&
    grep "dir1/sub1/b.txt" actual &&
    grep "dir2/c.txt" actual &&
    grep "root.txt" actual
'

test_expect_success 'diff --stat shows summary line' '
    (cd repo && grit diff --stat >../actual) &&
    grep "4 files changed" actual
'

test_expect_success 'diff --exit-code returns 1 for changes' '
    (cd repo && test_must_fail grit diff --exit-code)
'

test_expect_success 'diff -q suppresses output but returns exit code 1' '
    (cd repo && test_must_fail grit diff -q >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'stage changes and diff --cached shows correct patch' '
    (cd repo && grit add . && grit diff --cached >../actual) &&
    grep "^-root$" actual &&
    grep "^+root-mod$" actual &&
    grep "^-a$" actual &&
    grep "^+a-mod$" actual
'

test_expect_success 'diff --cached --name-only lists changed files' '
    (cd repo && grit diff --cached --name-only >../actual) &&
    printf "dir1/a.txt\ndir1/sub1/b.txt\ndir2/c.txt\nroot.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'diff --cached --name-status shows M' '
    (cd repo && grit diff --cached --name-status >../actual) &&
    grep "^M	dir1/a.txt$" actual &&
    grep "^M	root.txt$" actual
'

test_expect_success 'diff --cached --numstat counts correctly' '
    (cd repo && grit diff --cached --numstat >../actual) &&
    grep "^1	1	dir1/a.txt$" actual &&
    grep "^1	1	root.txt$" actual
'

test_expect_success 'diff --cached --stat summary' '
    (cd repo && grit diff --cached --stat >../actual) &&
    grep "4 files changed" actual &&
    grep "insertion" actual &&
    grep "deletion" actual
'

test_expect_success 'commit changes and verify tree diff' '
    (cd repo && grit commit -m "modify nested files" &&
     grit diff --name-only HEAD~1 HEAD >../actual) &&
    printf "dir1/a.txt\ndir1/sub1/b.txt\ndir2/c.txt\nroot.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'tree diff --name-status shows M for all' '
    (cd repo && grit diff --name-status HEAD~1 HEAD >../actual) &&
    grep "^M	dir1/a.txt$" actual &&
    grep "^M	dir1/sub1/b.txt$" actual &&
    grep "^M	dir2/c.txt$" actual &&
    grep "^M	root.txt$" actual
'

test_expect_success 'tree diff --numstat counts insertions and deletions' '
    (cd repo && grit diff --numstat HEAD~1 HEAD >../actual) &&
    grep "^1	1	dir1/a.txt$" actual &&
    grep "^1	1	dir2/c.txt$" actual
'

test_expect_success 'tree diff --stat shows files changed summary' '
    (cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "4 files changed" actual
'

test_expect_success 'tree diff with path filter on dir' '
    (cd repo && grit diff --name-only HEAD~1 HEAD -- dir1 >../actual) &&
    printf "dir1/a.txt\ndir1/sub1/b.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'tree diff with path filter on subdir' '
    (cd repo && grit diff --name-only HEAD~1 HEAD -- dir1/sub1 >../actual) &&
    echo "dir1/sub1/b.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'tree diff with path filter on single file' '
    (cd repo && grit diff --name-only HEAD~1 HEAD -- dir2/c.txt >../actual) &&
    echo "dir2/c.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'tree diff with multiple path filters' '
    (cd repo && grit diff --name-only HEAD~1 HEAD -- dir1/a.txt dir2/c.txt >../actual) &&
    printf "dir1/a.txt\ndir2/c.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'tree diff -U0 suppresses context lines' '
    (cd repo &&
     printf "line1\nline2\nline3\nline4\nline5\n" >root.txt &&
     grit add root.txt && grit commit -m "multiline" &&
     printf "line1\nline2\nMOD\nline4\nline5\n" >root.txt &&
     grit add root.txt && grit commit -m "modify middle" &&
     grit diff -U0 HEAD~1 HEAD >../actual) &&
    grep "^-line3$" actual &&
    grep "^+MOD$" actual &&
    ! grep "^ line2$" actual &&
    ! grep "^ line4$" actual
'

test_expect_success 'tree diff -U1 shows 1 context line' '
    (cd repo && grit diff -U1 HEAD~1 HEAD >../actual) &&
    grep "^ line2$" actual &&
    grep "^ line4$" actual &&
    ! grep "^ line1$" actual &&
    ! grep "^ line5$" actual
'

test_expect_success 'tree diff default context is 3 lines' '
    (cd repo &&
     printf "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n" >root.txt &&
     grit add root.txt && grit commit -m "ten lines" &&
     printf "1\n2\n3\n4\nX\n6\n7\n8\n9\n10\n" >root.txt &&
     grit add root.txt && grit commit -m "change line 5" &&
     grit diff HEAD~1 HEAD >../actual) &&
    grep "^ 2$" actual &&
    grep "^ 3$" actual &&
    grep "^ 4$" actual &&
    grep "^ 6$" actual &&
    grep "^ 7$" actual &&
    grep "^ 8$" actual &&
    ! grep "^ 1$" actual &&
    ! grep "^ 9$" actual
'

test_expect_success 'tree diff --exit-code returns 1 for changes' '
    (cd repo && test_must_fail grit diff --exit-code HEAD~1 HEAD)
'

test_expect_success 'tree diff --exit-code returns 0 for same commit' '
    (cd repo && grit diff --exit-code HEAD HEAD)
'

test_expect_success 'tree diff -q suppresses output' '
    (cd repo && test_must_fail grit diff -q HEAD~1 HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'add deeply nested structure and diff' '
    (cd repo && mkdir -p a/b/c/d/e &&
     echo "deep" >a/b/c/d/e/deep.txt &&
     grit add a/b/c/d/e/deep.txt &&
     grit commit -m "deep hierarchy" &&
     echo "deep-mod" >a/b/c/d/e/deep.txt &&
     grit add a/b/c/d/e/deep.txt &&
     grit commit -m "mod deep" &&
     grit diff --name-only HEAD~1 HEAD >../actual) &&
    echo "a/b/c/d/e/deep.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'tree diff path filter on deep subtree' '
    (cd repo && grit diff --name-only HEAD~1 HEAD -- a/b/c >../actual) &&
    echo "a/b/c/d/e/deep.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'diff patch has correct a/ b/ prefixes' '
    (cd repo && grit diff HEAD~1 HEAD >../actual) &&
    grep "^--- a/a/b/c/d/e/deep.txt$" actual &&
    grep "^+++ b/a/b/c/d/e/deep.txt$" actual
'

test_expect_success 'diff shows deletion in tree diff' '
    (cd repo && grit rm dir2/sub2/d.txt &&
     grit commit -m "remove d" &&
     grit diff --name-status HEAD~1 HEAD >../actual) &&
    echo "D	dir2/sub2/d.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'diff shows addition in tree diff' '
    (cd repo && mkdir -p dir2/sub2 && echo "new" >dir2/sub2/new.txt &&
     grit add dir2/sub2/new.txt &&
     grit commit -m "add new" &&
     grit diff --name-status HEAD~1 HEAD >../actual) &&
    echo "A	dir2/sub2/new.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'tree diff multiple files across dirs' '
    (cd repo &&
     echo "xx" >dir1/a.txt && echo "yy" >dir2/c.txt &&
     grit add . && grit commit -m "multi-dir mod" &&
     grit diff HEAD~1 HEAD >../actual) &&
    grep "^diff --git a/dir1/a.txt b/dir1/a.txt$" actual &&
    grep "^diff --git a/dir2/c.txt b/dir2/c.txt$" actual
'

test_done
