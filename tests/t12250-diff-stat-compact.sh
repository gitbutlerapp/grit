#!/bin/sh

test_description='diff --stat output format and summary lines'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
    grit init repo && cd repo &&
    git config user.email "t@t.com" && git config user.name "T" &&
    echo "hello" >file.txt &&
    grit add file.txt &&
    grit commit -m "initial"
	)
'

test_expect_success 'stat shows single modified file' '
    (cd repo && echo "modified" >file.txt &&
     grit add file.txt && grit commit -m "mod" &&
     grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "file.txt" actual &&
    grep "1 file changed" actual
'

test_expect_success 'stat shows insertion and deletion counts' '
    (cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "1 insertion" actual &&
    grep "1 deletion" actual
'

test_expect_success 'stat shows + and - markers' '
    (cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "+-$" actual
'

test_expect_success 'stat for pure addition' '
    (cd repo && echo "new" >new.txt && grit add new.txt && grit commit -m "add" &&
     grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "new.txt" actual &&
    grep "1 file changed" actual &&
    grep "1 insertion" actual &&
    ! grep "deletion" actual
'

test_expect_success 'stat + marker only for addition' '
    (cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "+$" actual
'

test_expect_success 'stat for pure deletion' '
    (cd repo && grit rm new.txt && grit commit -m "del" &&
     grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "new.txt" actual &&
    grep "1 file changed" actual &&
    grep "1 deletion" actual &&
    ! grep "insertion" actual
'

test_expect_success 'stat - marker only for deletion' '
    (cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "\-$" actual
'

test_expect_success 'stat with multiple files' '
    (cd repo && echo "a" >a.txt && echo "b" >b.txt && echo "c" >c.txt &&
     grit add . && grit commit -m "add abc" &&
     grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "a.txt" actual &&
    grep "b.txt" actual &&
    grep "c.txt" actual &&
    grep "3 files changed" actual
'

test_expect_success 'stat summary pluralization for 1 file' '
    (cd repo && echo "a2" >a.txt && grit add a.txt && grit commit -m "mod a" &&
     grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "1 file changed" actual
'

test_expect_success 'stat summary pluralization for multiple files' '
    (cd repo && echo "a3" >a.txt && echo "b3" >b.txt &&
     grit add . && grit commit -m "mod ab" &&
     grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "2 files changed" actual
'

test_expect_success 'stat with multiline additions' '
    (cd repo && printf "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n" >big.txt &&
     grit add big.txt && grit commit -m "add big" &&
     grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "big.txt" actual &&
    grep "10 insertions" actual
'

test_expect_success 'stat line count in change column' '
    (cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "10 " actual
'

test_expect_success 'stat with multiline replacement' '
    (cd repo && printf "1\n2\nX\nY\nZ\n6\n7\n8\n9\n10\n" >big.txt &&
     grit add big.txt && grit commit -m "mod big" &&
     grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "big.txt" actual &&
    grep "3 insertions" actual &&
    grep "3 deletions" actual
'

test_expect_success 'stat for cached diff' '
    (cd repo && echo "new-staged" >staged.txt && grit add staged.txt &&
     grit diff --cached --stat >../actual) &&
    grep "staged.txt" actual &&
    grep "1 file changed" actual
'

test_expect_success 'stat for cached diff with multiple staged' '
    (cd repo && echo "a4" >a.txt && grit add a.txt &&
     grit diff --cached --stat >../actual) &&
    grep "a.txt" actual &&
    grep "staged.txt" actual &&
    grep "2 files changed" actual
'

test_expect_success 'commit staged and stat for working tree diff' '
    (cd repo && grit commit -m "commit staged" &&
     echo "wt-mod" >file.txt &&
     grit diff --stat >../actual) &&
    grep "file.txt" actual
'

test_expect_success 'stat with nested directory files' '
    (cd repo && git checkout -- . &&
     mkdir -p dir/sub &&
     echo "nested" >dir/sub/n.txt &&
     grit add dir/sub/n.txt && grit commit -m "add nested" &&
     echo "nested-mod" >dir/sub/n.txt &&
     grit add dir/sub/n.txt && grit commit -m "mod nested" &&
     grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "dir/sub/n.txt" actual
'

test_expect_success 'stat with path filter' '
    (cd repo && echo "a5" >a.txt && echo "b5" >b.txt &&
     grit add . && grit commit -m "mod ab again" &&
     grit diff --stat HEAD~1 HEAD -- a.txt >../actual) &&
    grep "a.txt" actual &&
    ! grep "b.txt" actual &&
    grep "1 file changed" actual
'

test_expect_success 'stat with dir path filter' '
    (cd repo && echo "n2" >dir/sub/n.txt &&
     grit add . && grit commit -m "mod dir" &&
     grit diff --stat HEAD~1 HEAD -- dir >../actual) &&
    grep "dir/sub/n.txt" actual &&
    grep "1 file changed" actual
'

test_expect_success 'numstat shows machine-readable format' '
    (cd repo && grit diff --numstat HEAD~1 HEAD >../actual) &&
    grep "^1	1	dir/sub/n.txt$" actual
'

test_expect_success 'numstat for addition shows 0 deletions' '
    (cd repo && echo "z" >z.txt && grit add z.txt && grit commit -m "add z" &&
     grit diff --numstat HEAD~1 HEAD >../actual) &&
    echo "1	0	z.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'numstat for deletion shows 0 insertions' '
    (cd repo && grit rm z.txt && grit commit -m "del z" &&
     grit diff --numstat HEAD~1 HEAD >../actual) &&
    echo "0	1	z.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'numstat for multiple files' '
    (cd repo && echo "a6" >a.txt && echo "b6" >b.txt && echo "c6" >c.txt &&
     grit add . && grit commit -m "mod abc" &&
     grit diff --numstat HEAD~1 HEAD >../actual) &&
    test_line_count = 3 actual &&
    grep "^1	1	a.txt$" actual &&
    grep "^1	1	b.txt$" actual &&
    grep "^1	1	c.txt$" actual
'

test_expect_success 'stat for empty to content shows all insertions' '
    (cd repo && touch empty.txt && grit add empty.txt && grit commit -m "add empty" &&
     printf "one\ntwo\nthree\n" >empty.txt && grit add empty.txt && grit commit -m "fill" &&
     grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "empty.txt" actual &&
    grep "3 insertions" actual
'

test_expect_success 'stat for content to empty shows all deletions' '
    (cd repo && true >empty.txt && grit add empty.txt && grit commit -m "empty again" &&
     grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "empty.txt" actual &&
    grep "3 deletions" actual
'

test_expect_success 'stat shows no output for identical commits' '
    (cd repo && grit diff --stat HEAD HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'numstat shows no output for identical commits' '
    (cd repo && grit diff --numstat HEAD HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'stat with many added lines shows graph bar' '
    (cd repo && seq 1 50 >fifty.txt && grit add fifty.txt && grit commit -m "add fifty" &&
     grit diff --stat HEAD~1 HEAD >../actual) &&
    grep "fifty.txt" actual &&
    grep "50 insertions" actual &&
    grep "+++" actual
'

test_expect_success 'stat and numstat agree on file count' '
    (cd repo && echo "a7" >a.txt && echo "b7" >b.txt && echo "c7" >c.txt &&
     grit add . && grit commit -m "triple mod" &&
     grit diff --stat HEAD~1 HEAD >../actual_stat &&
     grit diff --numstat HEAD~1 HEAD >../actual_num) &&
    grep "3 files changed" actual_stat &&
    test_line_count = 3 actual_num
'

test_done
