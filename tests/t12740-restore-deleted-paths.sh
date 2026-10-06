#!/bin/sh
test_description='grit restore: deleted paths, --source, --staged, --worktree, --quiet, pathspecs'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

grit_status () {
    grit status --porcelain | grep -v "^##" || true
}

test_expect_success 'setup - repo with multiple commits' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     mkdir -p dir/sub &&
     echo v1 >file1.txt &&
     echo v1 >file2.txt &&
     echo v1 >file3.txt &&
     echo v1 >dir/nested.txt &&
     echo v1 >dir/sub/deep.txt &&
     grit add . &&
     grit commit -m "first" &&
     grit rev-parse HEAD >../first_hash &&
     echo v2 >file1.txt &&
     echo v2 >file2.txt &&
     echo v2 >file3.txt &&
     echo v2 >dir/nested.txt &&
     echo v2 >dir/sub/deep.txt &&
     grit add . &&
     grit commit -m "second" &&
     grit rev-parse HEAD >../second_hash)
'

test_expect_success 'restore worktree-deleted file from index' '
    (cd repo &&
     rm file1.txt &&
     grit restore file1.txt &&
     cat file1.txt >../actual) &&
    echo "v2" >expect &&
    test_cmp expect actual
'

test_expect_success 'restore multiple worktree-deleted files' '
    (cd repo &&
     rm file1.txt file2.txt &&
     grit restore file1.txt file2.txt &&
     cat file1.txt >../actual1 &&
     cat file2.txt >../actual2) &&
    echo "v2" >expect &&
    test_cmp expect actual1 &&
    test_cmp expect actual2
'

test_expect_success 'restore deleted file in subdirectory' '
    (cd repo &&
     rm dir/nested.txt &&
     grit restore dir/nested.txt &&
     cat dir/nested.txt >../actual) &&
    echo "v2" >expect &&
    test_cmp expect actual
'

test_expect_success 'restore deleted file in deep subdirectory' '
    (cd repo &&
     rm dir/sub/deep.txt &&
     grit restore dir/sub/deep.txt &&
     cat dir/sub/deep.txt >../actual) &&
    echo "v2" >expect &&
    test_cmp expect actual
'

test_expect_success 'restore --worktree explicitly on deleted file' '
    (cd repo &&
     rm file1.txt &&
     grit restore --worktree file1.txt &&
     cat file1.txt >../actual) &&
    echo "v2" >expect &&
    test_cmp expect actual
'

test_expect_success 'restore --source from older commit' '
    (cd repo &&
     grit restore --source "$(cat ../first_hash)" file1.txt &&
     cat file1.txt >../actual) &&
    echo "v1" >expect &&
    test_cmp expect actual
'

test_expect_success 'reset after source restore' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'restore --source with first commit hash restores v1' '
    (cd repo &&
     grit restore --source "$(cat ../first_hash)" file2.txt &&
     cat file2.txt >../actual) &&
    echo "v1" >expect &&
    test_cmp expect actual
'

test_expect_success 'reset after source restore of file2' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'restore --staged unstages a staged change' '
    (cd repo &&
     echo v3 >file1.txt &&
     grit add file1.txt &&
     grit restore --staged file1.txt &&
     grit_status >../actual) &&
    echo " M file1.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'reset after staged test' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'restore --staged --worktree restores both' '
    (cd repo &&
     echo v3 >file1.txt &&
     grit add file1.txt &&
     grit restore --staged --worktree file1.txt &&
     cat file1.txt >../actual &&
     grit_status >../status_out) &&
    echo "v2" >expect &&
    test_cmp expect actual &&
    ! grep "file1.txt" status_out
'

test_expect_success 'restore --source with --staged restores index to old version' '
    (cd repo &&
     grit restore --source "$(cat ../first_hash)" --staged file2.txt &&
     grit_status >../actual) &&
    grep "file2.txt" actual
'

test_expect_success 'reset after source staged' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'restore deleted file then verify status is clean' '
    (cd repo &&
     rm file3.txt &&
     grit restore file3.txt &&
     grit_status >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'restore modified file back to index version' '
    (cd repo &&
     echo modified >file1.txt &&
     grit restore file1.txt &&
     cat file1.txt >../actual) &&
    echo "v2" >expect &&
    test_cmp expect actual
'

test_expect_success 'restore with --quiet suppresses output' '
    (cd repo &&
     echo changed >file1.txt &&
     grit restore --quiet file1.txt >../actual 2>&1) &&
    test_must_be_empty actual
'

test_expect_success 'file is restored after quiet restore' '
    (cd repo && cat file1.txt >../actual) &&
    echo "v2" >expect &&
    test_cmp expect actual
'

test_expect_success 'restore all tracked files with dot pathspec' '
    (cd repo &&
     echo x >file1.txt &&
     echo x >file2.txt &&
     echo x >file3.txt &&
     grit restore . &&
     cat file1.txt >../a1 &&
     cat file2.txt >../a2 &&
     cat file3.txt >../a3) &&
    echo "v2" >expect &&
    test_cmp expect a1 &&
    test_cmp expect a2 &&
    test_cmp expect a3
'

test_expect_success 'restore --source on deleted file from older commit' '
    (cd repo &&
     rm file1.txt &&
     grit restore --source "$(cat ../first_hash)" file1.txt &&
     cat file1.txt >../actual) &&
    echo "v1" >expect &&
    test_cmp expect actual
'

test_expect_success 'reset to clean' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'restore staged deletion unstages it' '
    (cd repo &&
     grit rm file3.txt &&
     grit restore --staged file3.txt &&
     grit_status >../actual) &&
    echo " D file3.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'restore worktree after unstaging deletion' '
    (cd repo &&
     grit restore file3.txt &&
     cat file3.txt >../actual) &&
    echo "v2" >expect &&
    test_cmp expect actual
'

test_expect_success 'restore --staged --worktree on rm-ed file' '
    (cd repo &&
     grit rm file2.txt &&
     grit restore --staged --worktree file2.txt &&
     cat file2.txt >../actual &&
     grit_status >../status_out) &&
    echo "v2" >expect &&
    test_cmp expect actual &&
    ! grep "file2.txt" status_out
'

test_expect_success 'restore --source on directory path' '
    (cd repo &&
     echo changed >dir/nested.txt &&
     grit restore --source "$(cat ../first_hash)" dir/nested.txt &&
     cat dir/nested.txt >../actual) &&
    echo "v1" >expect &&
    test_cmp expect actual
'

test_expect_success 'reset after dir restore' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'restore --source on deep nested file' '
    (cd repo &&
     grit restore --source "$(cat ../first_hash)" dir/sub/deep.txt &&
     cat dir/sub/deep.txt >../actual) &&
    echo "v1" >expect &&
    test_cmp expect actual
'

test_expect_success 'reset after deep restore' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'create third commit for more tests' '
    (cd repo &&
     echo v3 >file1.txt &&
     grit add file1.txt &&
     grit commit -m "third" &&
     grit rev-parse HEAD >../third_hash)
'

test_expect_success 'restore --source first hash goes back two commits' '
    (cd repo &&
     grit restore --source "$(cat ../first_hash)" file1.txt &&
     cat file1.txt >../actual) &&
    echo "v1" >expect &&
    test_cmp expect actual
'

test_expect_success 'restore --source second hash goes back one commit' '
    (cd repo &&
     grit restore --source "$(cat ../second_hash)" file1.txt &&
     cat file1.txt >../actual) &&
    echo "v2" >expect &&
    test_cmp expect actual
'

test_expect_success 'reset to clean state' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'restore nonexistent path fails' '
    (cd repo &&
     test_must_fail grit restore nonexistent.txt 2>../err) &&
    test -s err
'

test_expect_success 'final state is clean' '
    (cd repo && grit_status >../actual) &&
    test_must_be_empty actual
'

test_done
