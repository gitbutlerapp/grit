#!/bin/sh

test_description='diff index (working tree vs staged, unstaged change detection)'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
    grit init repo && cd repo &&
    git config user.email "t@t.com" && git config user.name "T" &&
    echo "hello" >file.txt &&
    echo "world" >other.txt &&
    mkdir -p dir/sub &&
    echo "nested" >dir/nested.txt &&
    echo "deep" >dir/sub/deep.txt &&
    grit add . &&
    grit commit -m "initial"
	)
'

test_expect_success 'clean working tree has no diff' '
    (cd repo && grit diff >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'clean working tree exit-code is 0' '
    (cd repo && grit diff --exit-code)
'

test_expect_success 'modified file detected by name-only' '
    (cd repo && echo "hello2" >file.txt &&
     grit diff --name-only >../actual) &&
    echo "file.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'modified file shows M in name-status' '
    (cd repo && grit diff --name-status >../actual) &&
    echo "M	file.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'exit-code returns 1 with unstaged changes' '
    (cd repo && test_must_fail grit diff --exit-code)
'

test_expect_success 'quiet mode returns 1 with no output' '
    (cd repo && test_must_fail grit diff -q >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'multiple modified files detected' '
    (cd repo && echo "world2" >other.txt &&
     grit diff --name-only >../actual) &&
    printf "file.txt\nother.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'multiple modifications in name-status' '
    (cd repo && grit diff --name-status >../actual) &&
    grep "^M	file.txt$" actual &&
    grep "^M	other.txt$" actual
'

test_expect_success 'diff generates patch header' '
    (cd repo && grit diff >../actual) &&
    grep "^diff --git a/file.txt b/file.txt$" actual &&
    grep "^diff --git a/other.txt b/other.txt$" actual
'

test_expect_success 'diff patch has a/b prefix paths' '
    (cd repo && grit diff >../actual) &&
    grep "^--- a/file.txt$" actual &&
    grep "^+++ b/file.txt$" actual
'

test_expect_success 'diff patch shows deletion of old content' '
    (cd repo && grit diff >../actual) &&
    grep "^-hello$" actual &&
    grep "^-world$" actual
'

test_expect_success 'nested file modification detected' '
    (cd repo && git checkout -- . &&
     echo "nested2" >dir/nested.txt &&
     grit diff --name-only >../actual) &&
    echo "dir/nested.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'deeply nested file modification detected' '
    (cd repo && echo "deep2" >dir/sub/deep.txt &&
     grit diff --name-only >../actual) &&
    printf "dir/nested.txt\ndir/sub/deep.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'nested paths in name-status' '
    (cd repo && grit diff --name-status >../actual) &&
    grep "^M	dir/nested.txt$" actual &&
    grep "^M	dir/sub/deep.txt$" actual
'

test_expect_success 'stat output lists modified files' '
    (cd repo && grit diff --stat >../actual) &&
    grep "dir/nested.txt" actual &&
    grep "dir/sub/deep.txt" actual &&
    grep "2 files changed" actual
'

test_expect_success 'staging clears unstaged diff' '
    (cd repo && grit add dir/nested.txt dir/sub/deep.txt &&
     grit diff --name-only >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'staged changes visible with --cached' '
    (cd repo && grit diff --cached --name-only >../actual) &&
    printf "dir/nested.txt\ndir/sub/deep.txt\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'partially staged: working tree and cached differ' '
    (cd repo && echo "nested3" >dir/nested.txt &&
     grit diff --name-only >../actual_wt &&
     grit diff --cached --name-only >../actual_cached) &&
    echo "dir/nested.txt" >expect_wt &&
    test_cmp expect_wt actual_wt &&
    printf "dir/nested.txt\ndir/sub/deep.txt\n" >expect_cached &&
    test_cmp expect_cached actual_cached
'

test_expect_success 'new untracked file not shown in diff' '
    (cd repo && echo "untracked" >untracked.txt &&
     grit diff --name-only >../actual) &&
    ! grep "untracked.txt" actual
'

test_expect_success 'commit and verify clean state' '
    (cd repo && grit add . && grit commit -m "update" &&
     grit diff --exit-code)
'

test_expect_success 'deleted file not detected by unstaged diff until staged' '
    (cd repo && rm other.txt &&
     grit diff --name-only >../actual) &&
    echo "other.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'deleted file shows M in working tree diff' '
    (cd repo && grit diff --name-status >../actual) &&
    grep "other.txt" actual
'

test_expect_success 'stage deletion and verify cached shows D' '
    (cd repo && grit rm other.txt &&
     grit diff --cached --name-status >../actual) &&
    echo "D	other.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'working tree diff clean after staging deletion' '
    (cd repo && grit diff --name-only >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'commit deletion and verify' '
    (cd repo && grit commit -m "del other" &&
     grit diff --exit-code)
'

test_expect_success 'diff with mode change content stays same' '
    (cd repo && chmod +x file.txt &&
     grit diff --name-only >../actual) &&
    echo "file.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'reset mode and verify clean' '
    (cd repo && chmod -x file.txt &&
     grit diff --exit-code)
'

test_expect_success 'stat for working tree diff counts deletions' '
    (cd repo && echo "changed" >file.txt &&
     grit diff --stat >../actual) &&
    grep "file.txt" actual &&
    grep "1 file changed" actual
'

test_expect_success 'numstat for working tree diff' '
    (cd repo && grit diff --numstat >../actual) &&
    grep "file.txt" actual
'

test_expect_success 'reset and add large file' '
    (cd repo && git checkout -- . &&
     seq 1 100 >big.txt && grit add big.txt && grit commit -m "add big" &&
     seq 1 50 >big.txt && seq 60 100 >>big.txt &&
     grit diff --name-only >../actual) &&
    echo "big.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'stat on large file change' '
    (cd repo && grit diff --stat >../actual) &&
    grep "big.txt" actual
'

test_done
