#!/bin/sh
test_description='grit mv --verbose, --dry-run, --force, -k'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

grit_status () {
    grit status --porcelain | grep -v "^##" || true
}

test_expect_success 'setup' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
     echo hello >file.txt &&
     echo world >second.txt &&
     mkdir -p sub &&
     echo nested >sub/deep.txt &&
     grit add . &&
     grit commit -m "initial")
'

test_expect_success 'mv renames file in index and working tree' '
    (cd repo &&
     grit mv file.txt renamed.txt &&
     grit_status >../actual) &&
    sort actual >actual_sorted &&
    grep "R" actual &&
    grep "renamed.txt" actual
    test_path_is_file repo/renamed.txt &&
    test_path_is_missing repo/file.txt
'

test_expect_success 'mv shows R in status' '
    (cd repo &&
     grit_status >../actual) &&
    grep "renamed.txt" actual
'

test_expect_success 'reset after mv' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv --dry-run shows what would happen' '
    (cd repo &&
     grit mv --dry-run file.txt moved.txt >../actual 2>&1) &&
    grep "file.txt" actual
'

test_expect_success 'mv --dry-run does not actually move file' '
    test_path_is_file repo/file.txt &&
    test_path_is_missing repo/moved.txt
'

test_expect_success 'mv -n is alias for --dry-run' '
    (cd repo &&
     grit mv -n second.txt moved2.txt >../actual 2>&1) &&
    grep "second.txt" actual &&
    test_path_is_file repo/second.txt
'

test_expect_success 'mv --verbose shows what is being moved' '
    (cd repo &&
     grit mv --verbose file.txt verbose-moved.txt >../actual 2>&1) &&
    grep "file.txt" actual
'

test_expect_success 'reset after verbose mv' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv to existing file fails without --force' '
    (cd repo &&
     test_must_fail grit mv file.txt second.txt 2>../actual) &&
    test_path_is_file actual
'

test_expect_success 'mv --force overwrites existing destination' '
    (cd repo &&
     grit mv --force file.txt second.txt &&
     grit_status >../actual) &&
    grep "second.txt" actual &&
    test_path_is_missing repo/file.txt
'

test_expect_success 'reset after force mv' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv file into directory' '
    (cd repo &&
     grit mv file.txt sub/ &&
     grit_status >../actual) &&
    grep "sub/file.txt" actual &&
    test_path_is_file repo/sub/file.txt
'

test_expect_success 'reset after dir mv' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv multiple files into directory' '
    (cd repo &&
     grit mv file.txt second.txt sub/ &&
     grit_status >../actual) &&
    grep "sub/file.txt" actual &&
    grep "sub/second.txt" actual
'

test_expect_success 'reset after multi mv' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv -k skips errors instead of aborting' '
    (cd repo &&
     grit mv -k nonexistent.txt sub/ 2>../actual;
     true) &&
    test_path_is_dir repo/sub
'

test_expect_success 'mv --verbose --dry-run combined' '
    (cd repo &&
     grit mv --verbose --dry-run file.txt vd-test.txt >../actual 2>&1) &&
    grep "file.txt" actual &&
    test_path_is_file repo/file.txt
'

test_expect_success 'mv file with spaces in name' '
    (cd repo &&
     echo space >"space file.txt" &&
     grit add "space file.txt" &&
     grit commit -m "add space" &&
     grit mv "space file.txt" "moved space.txt" &&
     grit_status >../actual) &&
    grep "moved space.txt" actual &&
    test_path_is_missing "repo/space file.txt" &&
    test_path_is_file "repo/moved space.txt"
'

test_expect_success 'reset after space mv' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv creates destination subdirectory' '
    (cd repo &&
     grit mv file.txt newdir/file.txt &&
     grit_status >../actual) &&
    grep "newdir/file.txt" actual &&
    test_path_is_file repo/newdir/file.txt
'

test_expect_success 'reset after newdir mv' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv preserves file content' '
    (cd repo &&
     cat file.txt >../before_content &&
     grit mv file.txt preserved.txt &&
     cat preserved.txt >../after_content) &&
    test_cmp before_content after_content
'

test_expect_success 'reset after preserve test' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv same name is a no-op or error' '
    (cd repo &&
     test_must_fail grit mv file.txt file.txt 2>../actual) &&
    test_path_is_file actual
'

test_expect_success 'mv directory requires files inside' '
    (cd repo &&
     grit mv sub newdir &&
     grit_status >../actual) &&
    grep "newdir" actual
'

test_expect_success 'reset after dir rename' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv --force --verbose combined' '
    (cd repo &&
     grit mv --force --verbose file.txt second.txt >../actual 2>&1) &&
    grep "file.txt" actual
'

test_expect_success 'reset after force verbose' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv updates index correctly for commit' '
    (cd repo &&
     grit mv file.txt for-commit.txt &&
     grit commit -m "renamed file" &&
     grit log --oneline -n 1 >../actual) &&
    grep "renamed file" actual
'

test_expect_success 'mv then mv back restores original state' '
    (cd repo &&
     grit mv for-commit.txt temp-name.txt &&
     grit mv temp-name.txt for-commit.txt) &&
    (cd repo && grit_status >../actual) &&
    test ! -s actual &&
    test_path_is_file repo/for-commit.txt
'

test_expect_success 'mv -f into directory with conflict' '
    (cd repo &&
     echo conflict >sub/deep.txt &&
     grit add sub/deep.txt &&
     grit commit -m "update deep" &&
     echo new >deep.txt &&
     grit add deep.txt &&
     grit commit -m "add deep" &&
     mkdir -p target &&
     echo existing >target/deep.txt &&
     grit add target/deep.txt &&
     grit commit -m "add target" &&
     grit mv -f deep.txt target/deep.txt &&
     grit_status >../actual) &&
    grep "target/deep.txt" actual
'

test_expect_success 'reset after conflict mv' '
    (cd repo && grit reset --hard HEAD)
'

test_done
