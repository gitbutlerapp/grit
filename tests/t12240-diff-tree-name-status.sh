#!/bin/sh

test_description='diff --name-status between tree objects (commit-to-commit)'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
    grit init repo && cd repo &&
    git config user.email "t@t.com" && git config user.name "T" &&
    echo "a" >a.txt &&
    echo "b" >b.txt &&
    echo "c" >c.txt &&
    mkdir -p dir/sub &&
    echo "d" >dir/d.txt &&
    echo "e" >dir/sub/e.txt &&
    grit add . &&
    grit commit -m "initial"
	)
'

test_expect_success 'no diff between same commit' '
    (cd repo && grit diff --name-status HEAD HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'modified file shows M status' '
    (cd repo && echo "a-mod" >a.txt && grit add a.txt && grit commit -m "mod a" &&
     grit diff --name-status HEAD~1 HEAD >../actual) &&
    echo "M	a.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'added file shows A status' '
    (cd repo && echo "new" >new.txt && grit add new.txt && grit commit -m "add new" &&
     grit diff --name-status HEAD~1 HEAD >../actual) &&
    echo "A	new.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'deleted file shows D status' '
    (cd repo && grit rm c.txt && grit commit -m "del c" &&
     grit diff --name-status HEAD~1 HEAD >../actual) &&
    echo "D	c.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'multiple modifications in one commit' '
    (cd repo && echo "a2" >a.txt && echo "b2" >b.txt &&
     grit add a.txt b.txt && grit commit -m "mod a and b" &&
     grit diff --name-status HEAD~1 HEAD >../actual) &&
    printf "M\ta.txt\nM\tb.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'mixed add/modify/delete' '
    (cd repo && echo "b3" >b.txt && grit rm new.txt &&
     echo "fresh" >fresh.txt && grit add b.txt fresh.txt &&
     grit commit -m "mixed ops" &&
     grit diff --name-status HEAD~1 HEAD >../actual) &&
    printf "M\tb.txt\nD\tnew.txt\nA\tfresh.txt\n" >expect &&
    sort expect >expect.sorted &&
    sort actual >actual.sorted &&
    test_cmp expect.sorted actual.sorted
'

test_expect_success 'nested file modification shows correct path' '
    (cd repo && echo "d-mod" >dir/d.txt && grit add dir/d.txt &&
     grit commit -m "mod nested" &&
     grit diff --name-status HEAD~1 HEAD >../actual) &&
    echo "M	dir/d.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'deeply nested file modification' '
    (cd repo && echo "e-mod" >dir/sub/e.txt && grit add dir/sub/e.txt &&
     grit commit -m "mod deep" &&
     grit diff --name-status HEAD~1 HEAD >../actual) &&
    echo "M	dir/sub/e.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'add file in nested dir shows A' '
    (cd repo && echo "f" >dir/sub/f.txt && grit add dir/sub/f.txt &&
     grit commit -m "add f" &&
     grit diff --name-status HEAD~1 HEAD >../actual) &&
    echo "A	dir/sub/f.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'delete file in nested dir shows D' '
    (cd repo && grit rm dir/sub/f.txt && grit commit -m "del f" &&
     grit diff --name-status HEAD~1 HEAD >../actual) &&
    echo "D	dir/sub/f.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'name-only shows just paths' '
    (cd repo && echo "a3" >a.txt && echo "b4" >b.txt &&
     grit add . && grit commit -m "mod a b again" &&
     grit diff --name-only HEAD~1 HEAD >../actual) &&
    printf "a.txt\nb.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'name-status with path filter' '
    (cd repo && grit diff --name-status HEAD~1 HEAD -- a.txt >../actual) &&
    echo "M	a.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'name-status with dir path filter' '
    (cd repo && echo "d2" >dir/d.txt && echo "e2" >dir/sub/e.txt &&
     grit add . && grit commit -m "mod dir files" &&
     grit diff --name-status HEAD~1 HEAD -- dir >../actual) &&
    printf "M\tdir/d.txt\nM\tdir/sub/e.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'name-only with dir path filter' '
    (cd repo && grit diff --name-only HEAD~1 HEAD -- dir >../actual) &&
    printf "dir/d.txt\ndir/sub/e.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'name-status with subdir path filter' '
    (cd repo && grit diff --name-status HEAD~1 HEAD -- dir/sub >../actual) &&
    echo "M	dir/sub/e.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'diff across multiple commits with name-status' '
    (cd repo &&
     grit diff --name-status HEAD~3 HEAD >../actual) &&
    grep "^M	a.txt$" actual &&
    grep "^M	b.txt$" actual &&
    grep "^M	dir/d.txt$" actual
'

test_expect_success 'stat output shows file summary' '
    (cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "dir/d.txt" actual &&
    grep "dir/sub/e.txt" actual &&
    grep "2 files changed" actual
'

test_expect_success 'numstat output for tree diff' '
    (cd repo && grit diff --numstat HEAD~1 HEAD >../actual) &&
    grep "^1	1	dir/d.txt$" actual &&
    grep "^1	1	dir/sub/e.txt$" actual
'

test_expect_success 'exit-code returns 1 for differing commits' '
    (cd repo && test_must_fail grit diff --exit-code HEAD~1 HEAD)
'

test_expect_success 'exit-code returns 0 for identical commits' '
    (cd repo && grit diff --exit-code HEAD HEAD)
'

test_expect_success 'quiet mode returns exit code only' '
    (cd repo && test_must_fail grit diff -q HEAD~1 HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'diff shows full patch between commits' '
    (cd repo && grit diff HEAD~1 HEAD >../actual) &&
    grep "^diff --git a/dir/d.txt b/dir/d.txt$" actual &&
    grep "^diff --git a/dir/sub/e.txt b/dir/sub/e.txt$" actual
'

test_expect_success 'diff patch has correct content lines' '
    (cd repo && grit diff HEAD~1 HEAD >../actual) &&
    grep "^-d-mod$" actual &&
    grep "^+d2$" actual
'

test_expect_success 'add many files and verify all shown in name-status' '
    (cd repo && mkdir -p multi &&
     echo "1" >multi/f1.txt && echo "2" >multi/f2.txt &&
     echo "3" >multi/f3.txt && echo "4" >multi/f4.txt &&
     echo "5" >multi/f5.txt &&
     grit add multi && grit commit -m "add multi" &&
     grit diff --name-status HEAD~1 HEAD >../actual) &&
    grep "^A	multi/f1.txt$" actual &&
    grep "^A	multi/f2.txt$" actual &&
    grep "^A	multi/f3.txt$" actual &&
    grep "^A	multi/f4.txt$" actual &&
    grep "^A	multi/f5.txt$" actual
'

test_expect_success 'name-status count matches expected additions' '
    (cd repo && grit diff --name-status HEAD~1 HEAD >../actual) &&
    test_line_count = 5 actual
'

test_expect_success 'modify all multi files and verify' '
    (cd repo &&
     echo "1m" >multi/f1.txt && echo "2m" >multi/f2.txt &&
     echo "3m" >multi/f3.txt && echo "4m" >multi/f4.txt &&
     echo "5m" >multi/f5.txt &&
     grit add multi && grit commit -m "mod multi" &&
     grit diff --name-status HEAD~1 HEAD >../actual) &&
    grep "^M	multi/f1.txt$" actual &&
    grep "^M	multi/f5.txt$" actual &&
    test_line_count = 5 actual
'

test_expect_success 'delete all multi files and verify D status' '
    (cd repo && grit rm multi/f1.txt multi/f2.txt multi/f3.txt multi/f4.txt multi/f5.txt &&
     grit commit -m "del multi" &&
     grit diff --name-status HEAD~1 HEAD >../actual) &&
    grep "^D	multi/f1.txt$" actual &&
    grep "^D	multi/f5.txt$" actual &&
    test_line_count = 5 actual
'

test_expect_success 'stat for deletion shows removed lines' '
    (cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "5 files changed" actual &&
    grep "deletion" actual
'

test_expect_success 'numstat for deletion shows 0 insertions' '
    (cd repo && grit diff --numstat HEAD~1 HEAD >../actual) &&
    grep "^0	1	multi/f1.txt$" actual
'

test_expect_success 'diff across many commits' '
    (cd repo && grit diff --name-only HEAD~2 HEAD >../actual) &&
    grep "multi/f1.txt" actual
'

test_expect_success 'stat with path filter' '
    (cd repo && echo "a-final" >a.txt && echo "b-final" >b.txt &&
     grit add . && grit commit -m "final mods" &&
     grit diff --stat HEAD~1 HEAD -- a.txt >../actual) &&
    grep "a.txt" actual &&
    grep "1 file changed" actual
'

test_done
