#!/bin/sh
test_description='grit rm --force, -r, --cached, --dry-run, --quiet, --ignore-unmatch'
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
     echo hello >file.txt &&
     echo world >second.txt &&
     mkdir -p sub/deep &&
     echo a >sub/a.txt &&
     echo b >sub/deep/b.txt &&
     grit add . &&
     grit commit -m "initial")
'

test_expect_success 'rm removes file from index and working tree' '
    (cd repo &&
     grit rm second.txt &&
     grit_status >../actual) &&
    echo "D  second.txt" >expect &&
    test_cmp expect actual &&
    test_path_is_missing repo/second.txt
'

test_expect_success 'rm --cached removes from index but keeps working tree' '
    (cd repo &&
     grit reset --hard HEAD &&
     grit rm --cached file.txt &&
     grit_status >../actual) &&
    grep "D  file.txt" actual &&
    test_path_is_file repo/file.txt
'

test_expect_success 'reset after cached rm' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm directory fails without -r' '
    (cd repo &&
     test_must_fail grit rm sub 2>../actual) &&
    test_path_is_file actual
'

test_expect_success 'rm -r removes directory recursively' '
    (cd repo &&
     grit rm -r sub &&
     grit_status >../actual) &&
    grep "^D" actual | sort >actual_del &&
    cat >expect <<-\EOF &&
	D  sub/a.txt
	D  sub/deep/b.txt
	EOF
    test_cmp expect actual_del
'

test_expect_success 'rm -r removes files from working tree' '
    test_path_is_missing repo/sub/a.txt &&
    test_path_is_missing repo/sub/deep/b.txt
'

test_expect_success 'reset after recursive rm' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm --dry-run shows what would be removed' '
    (cd repo &&
     grit rm --dry-run file.txt >../actual 2>&1) &&
    grep "file.txt" actual
'

test_expect_success 'rm --dry-run does not actually remove from index' '
    test_path_is_file repo/file.txt
'

test_expect_success 'rm -n is alias for --dry-run' '
    (cd repo &&
     grit rm -n second.txt >../actual 2>&1) &&
    grep "second.txt" actual &&
    test_path_is_file repo/second.txt
'

test_expect_success 'rm --quiet suppresses output' '
    (cd repo &&
     grit rm --quiet file.txt >../actual 2>&1) &&
    test ! -s actual
'

test_expect_success 'reset after quiet rm' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm nonexistent file fails' '
    (cd repo &&
     test_must_fail grit rm nonexistent.txt 2>../actual) &&
    test_path_is_file actual
'

test_expect_success 'rm --ignore-unmatch with nonexistent succeeds' '
    (cd repo &&
     grit rm --ignore-unmatch nonexistent.txt 2>../actual) &&
    true
'

test_expect_success 'rm file with staged modifications fails without --force' '
    (cd repo &&
     echo modified >file.txt &&
     grit add file.txt &&
     test_must_fail grit rm file.txt 2>../actual) &&
    test_path_is_file actual
'

test_expect_success 'rm --force removes file with staged modifications' '
    (cd repo &&
     grit rm --force file.txt &&
     grit_status >../actual) &&
    echo "D  file.txt" >expect &&
    test_cmp expect actual &&
    test_path_is_missing repo/file.txt
'

test_expect_success 'reset after force rm' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm --cached with staged modifications keeps working tree' '
    (cd repo &&
     echo changed >file.txt &&
     grit add file.txt &&
     grit rm --cached file.txt &&
     grit_status >../actual) &&
    grep "D  file.txt" actual &&
    test_path_is_file repo/file.txt
'

test_expect_success 'reset after cached staged rm' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm multiple files at once' '
    (cd repo &&
     grit rm file.txt second.txt &&
     grit_status >../actual) &&
    sort actual >actual_sorted &&
    cat >expect <<-\EOF &&
	D  file.txt
	D  second.txt
	EOF
    test_cmp expect actual_sorted &&
    test_path_is_missing repo/file.txt &&
    test_path_is_missing repo/second.txt
'

test_expect_success 'reset after multi rm' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm -r --cached keeps working tree for directory' '
    (cd repo &&
     grit rm -r --cached sub &&
     grit_status >../actual) &&
    grep "^D" actual | sort >actual_del &&
    cat >expect <<-\EOF &&
	D  sub/a.txt
	D  sub/deep/b.txt
	EOF
    test_cmp expect actual_del &&
    test_path_is_file repo/sub/a.txt
'

test_expect_success 'reset after cached recursive rm' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm -r --dry-run for directory' '
    (cd repo &&
     grit rm -r --dry-run sub >../actual 2>&1) &&
    grep "sub" actual &&
    test_path_is_file repo/sub/a.txt &&
    test_path_is_file repo/sub/deep/b.txt
'

test_expect_success 'rm --force --cached with staged mods' '
    (cd repo &&
     echo new-content >file.txt &&
     grit add file.txt &&
     grit rm --force --cached file.txt &&
     grit_status >../actual) &&
    grep "D  file.txt" actual &&
    test_path_is_file repo/file.txt
'

test_expect_success 'reset after force cached rm' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm file then re-add it' '
    (cd repo &&
     grit rm second.txt &&
     echo re-added >second.txt &&
     grit add second.txt &&
     grit_status >../actual) &&
    grep "M  second.txt" actual
'

test_expect_success 'reset after re-add' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm with file in subdirectory by full path' '
    (cd repo &&
     grit rm sub/a.txt &&
     grit_status >../actual) &&
    echo "D  sub/a.txt" >expect &&
    test_cmp expect actual &&
    test_path_is_missing repo/sub/a.txt
'

test_expect_success 'reset after subdir rm' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm --quiet --force removes silently' '
    (cd repo &&
     echo mod >file.txt &&
     grit add file.txt &&
     grit rm --quiet --force file.txt >../actual 2>&1) &&
    test ! -s actual &&
    test_path_is_missing repo/file.txt
'

test_expect_success 'reset after quiet force rm' '
    (cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm -r --force removes dir with staged changes' '
    (cd repo &&
     echo changed >sub/a.txt &&
     grit add sub/a.txt &&
     grit rm -r --force sub &&
     grit_status >../actual) &&
    grep "^D" actual | sort >actual_del &&
    cat >expect <<-\EOF &&
	D  sub/a.txt
	D  sub/deep/b.txt
	EOF
    test_cmp expect actual_del
'

test_expect_success 'reset final' '
    (cd repo && grit reset --hard HEAD)
'

test_done
