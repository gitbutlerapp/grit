#!/bin/sh
test_description='grit reset: --mixed with paths, --soft, --hard, --quiet, path-based reset'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

grit_status () {
    grit status --porcelain | grep -v "^##" || true
}

test_expect_success 'setup - repo with three commits' '
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
     grit rev-parse HEAD >../second_hash &&
     echo v3 >file1.txt &&
     echo v3 >file2.txt &&
     grit add . &&
     grit commit -m "third" &&
     grit rev-parse HEAD >../third_hash)
'

# === PATH-BASED RESET (always mixed behavior) ===

test_expect_success 'reset HEAD -- path unstages a staged change' '
    (cd repo &&
     echo v4 >file1.txt &&
     grit add file1.txt &&
     grit reset HEAD -- file1.txt &&
     grit_status >../actual) &&
    echo " M file1.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'worktree keeps the modification after path reset' '
    (cd repo && cat file1.txt >../actual) &&
    echo "v4" >expect &&
    test_cmp expect actual
'

test_expect_success 'restore to clean state' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'reset HEAD -- multiple paths' '
    (cd repo &&
     echo v4 >file1.txt &&
     echo v4 >file2.txt &&
     grit add file1.txt file2.txt &&
     grit reset HEAD -- file1.txt file2.txt &&
     grit_status >../actual) &&
    sort actual >actual_sorted &&
    cat >expect <<-\EOF &&
	 M file1.txt
	 M file2.txt
	EOF
    sort expect >expect_sorted &&
    test_cmp expect_sorted actual_sorted
'

test_expect_success 'restore after multi-path reset' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'reset HEAD -- on nested path' '
    (cd repo &&
     echo v4 >dir/nested.txt &&
     grit add dir/nested.txt &&
     grit reset HEAD -- dir/nested.txt &&
     grit_status >../actual) &&
    echo " M dir/nested.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'restore after nested path reset' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'reset HEAD -- on deep nested path' '
    (cd repo &&
     echo v4 >dir/sub/deep.txt &&
     grit add dir/sub/deep.txt &&
     grit reset HEAD -- dir/sub/deep.txt &&
     grit_status >../actual) &&
    echo " M dir/sub/deep.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'restore after deep nested reset' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'reset to older commit on specific path' '
    (cd repo &&
     grit reset "$(cat ../first_hash)" -- file1.txt &&
     grit_status >../actual) &&
    grep "file1.txt" actual
'

test_expect_success 'index has v1 but worktree still has v3' '
    (cd repo && cat file1.txt >../actual) &&
    echo "v3" >expect &&
    test_cmp expect actual
'

test_expect_success 'restore after older commit path reset' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'reset path on newly added file unstages it' '
    (cd repo &&
     echo new >new.txt &&
     grit add new.txt &&
     grit reset HEAD -- new.txt &&
     grit_status >../actual) &&
    echo "?? new.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'cleanup new file' '
    (cd repo && rm -f new.txt)
'

# === SOFT RESET ===

test_expect_success 'reset --soft moves HEAD but keeps index and worktree' '
    (cd repo &&
     grit reset --soft "$(cat ../second_hash)" &&
     grit rev-parse HEAD >../head_actual) &&
    test_cmp second_hash head_actual
'

test_expect_success 'after soft reset worktree still has v3 content' '
    (cd repo && cat file1.txt >../actual) &&
    echo "v3" >expect &&
    test_cmp expect actual
'

test_expect_success 'after soft reset changes are staged' '
    (cd repo && grit_status >../actual) &&
    grep "^M  file1.txt" actual
'

test_expect_success 'restore to third commit' '
    (cd repo && grit reset --hard "$(cat ../third_hash)")
'

# === MIXED RESET (default) ===

test_expect_success 'reset --mixed resets index but keeps worktree' '
    (cd repo &&
     grit reset --mixed "$(cat ../second_hash)" &&
     cat file1.txt >../wt_actual &&
     grit_status >../status_actual) &&
    echo "v3" >wt_expect &&
    test_cmp wt_expect wt_actual &&
    grep " M file1.txt" status_actual
'

test_expect_success 'recommit for next test' '
    (cd repo &&
     grit add . &&
     grit commit -m "re-third" &&
     grit rev-parse HEAD >../third_hash)
'

test_expect_success 'reset with no mode flag defaults to --mixed' '
    (cd repo &&
     grit reset "$(cat ../second_hash)" &&
     cat file1.txt >../wt_actual &&
     grit_status >../status_actual) &&
    echo "v3" >wt_expect &&
    test_cmp wt_expect wt_actual &&
    grep " M file1.txt" status_actual
'

test_expect_success 'recommit again' '
    (cd repo &&
     grit add . &&
     grit commit -m "re-third-2" &&
     grit rev-parse HEAD >../third_hash)
'

# === HARD RESET ===

test_expect_success 'reset --hard resets index and worktree' '
    (cd repo &&
     grit reset --hard "$(cat ../second_hash)" &&
     cat file1.txt >../actual &&
     grit_status >../status_actual) &&
    echo "v2" >expect &&
    test_cmp expect actual &&
    test_must_be_empty status_actual
'

test_expect_success 'HEAD points to second commit after hard reset' '
    (cd repo && grit rev-parse HEAD >../actual) &&
    test_cmp second_hash actual
'

test_expect_success 'reset --hard HEAD cleans modified worktree' '
    (cd repo &&
     echo dirty >file1.txt &&
     grit reset --hard HEAD &&
     cat file1.txt >../actual) &&
    echo "v2" >expect &&
    test_cmp expect actual
'

test_expect_success 'reset --hard HEAD with staged changes discards them' '
    (cd repo &&
     echo staged-dirty >file2.txt &&
     grit add file2.txt &&
     grit reset --hard HEAD &&
     cat file2.txt >../actual) &&
    echo "v2" >expect &&
    test_cmp expect actual
'

# === QUIET ===

test_expect_success 'reset --quiet suppresses output' '
    (cd repo &&
     echo dirty >file1.txt &&
     grit add file1.txt &&
     grit reset --quiet HEAD -- file1.txt >../actual 2>&1) &&
    test_must_be_empty actual
'

test_expect_success 'cleanup quiet test' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'reset --hard --quiet also suppresses' '
    (cd repo &&
     echo dirty >file1.txt &&
     grit add file1.txt &&
     grit reset --hard --quiet HEAD >../actual 2>&1) &&
    test_must_be_empty actual
'

# === EDGE CASES ===

test_expect_success 'reset HEAD with no paths and no mode is mixed' '
    (cd repo &&
     echo v99 >file1.txt &&
     grit add file1.txt &&
     grit reset HEAD &&
     grit_status >../actual) &&
    echo " M file1.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'cleanup edge case' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'reset to first commit and verify all files reverted' '
    (cd repo &&
     grit reset --hard "$(cat ../first_hash)" &&
     cat file1.txt >../a1 &&
     cat file2.txt >../a2 &&
     cat file3.txt >../a3) &&
    echo "v1" >expect &&
    test_cmp expect a1 &&
    test_cmp expect a2 &&
    test_cmp expect a3
'

test_expect_success 'final state is clean' '
    (cd repo && grit_status >../actual) &&
    test_must_be_empty actual
'

test_done
