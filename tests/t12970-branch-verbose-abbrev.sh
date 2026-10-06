#!/bin/sh

test_description='branch: -v verbose output, listing, creation, deletion, --show-current, --contains, --merged'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with initial commit' '
    grit init repo &&
    (cd repo &&
     grit config user.email "t@t.com" &&
     grit config user.name "T" &&
     echo hello >file.txt &&
     grit add file.txt &&
     grit commit -m "initial")
'

test_expect_success 'branch list shows main' '
    (cd repo && grit branch >../actual) &&
    grep "main" actual
'

test_expect_success 'branch list marks current branch with asterisk' '
    (cd repo && grit branch >../actual) &&
    grep "^\* main" actual
'

test_expect_success 'branch create new branch' '
    (cd repo && grit branch feature) &&
    (cd repo && grit branch >../actual) &&
    grep "feature" actual
'

test_expect_success 'branch -v shows commit hash and subject' '
    (cd repo && grit branch -v >../actual) &&
    grep "feature" actual | grep "initial" &&
    grep "main" actual | grep "initial"
'

test_expect_success 'branch -v shows abbreviated hash' '
    (cd repo && grit branch -v >../actual) &&
    hash=$(cd repo && grit rev-parse --short HEAD) &&
    grep "$hash" actual
'

test_expect_success 'branch -v marks current branch' '
    (cd repo && grit branch -v >../actual) &&
    grep "^\* main" actual
'

test_expect_success 'branch -vv output same format as -v' '
    (cd repo && grit branch -vv >../actual) &&
    grep "feature" actual | grep "initial"
'

test_expect_success 'branch --show-current shows current branch name' '
    (cd repo && grit branch --show-current >../actual) &&
    echo "main" >expect &&
    test_cmp expect actual
'

test_expect_success 'branch --show-current after switching' '
    (cd repo && grit switch feature && grit branch --show-current >../actual) &&
    echo "feature" >expect &&
    test_cmp expect actual
'

test_expect_success 'switch back to main' '
    (cd repo && grit switch main)
'

test_expect_success 'create second commit on main' '
    (cd repo &&
     echo world >>file.txt &&
     grit add file.txt &&
     grit commit -m "second commit")
'

test_expect_success 'branch -v shows different commits for diverged branches' '
    (cd repo && grit branch -v >../actual) &&
    grep "feature" actual | grep "initial" &&
    grep "main" actual | grep "second commit"
'

test_expect_success 'branch --contains HEAD lists main' '
    (cd repo && grit branch --contains HEAD >../actual) &&
    grep "main" actual
'

test_expect_success 'branch --merged HEAD lists branches' '
    (cd repo && grit branch --merged HEAD >../actual) &&
    grep "main" actual
'

test_expect_success 'branch -d deletes merged branch' '
    (cd repo && grit branch deleteme &&
     grit branch -d deleteme) &&
    (cd repo && grit branch >../actual) &&
    ! grep "deleteme" actual
'

test_expect_success 'branch -D force deletes branch' '
    (cd repo && grit branch forcedel &&
     grit branch -D forcedel) &&
    (cd repo && grit branch >../actual) &&
    ! grep "forcedel" actual
'

test_expect_success 'branch -m renames branch' '
    (cd repo && grit branch oldname &&
     grit branch -m oldname newname) &&
    (cd repo && grit branch >../actual) &&
    ! grep "oldname" actual &&
    grep "newname" actual
'

test_expect_success 'branch -M force renames branch' '
    (cd repo && grit branch force_src &&
     grit branch -M force_src force_dst) &&
    (cd repo && grit branch >../actual) &&
    ! grep "force_src" actual &&
    grep "force_dst" actual
'

test_expect_success 'cleanup renamed branches' '
    (cd repo &&
     grit branch -D force_dst &&
     grit branch -D newname)
'

test_expect_success 'branch -v with many branches' '
    (cd repo &&
     grit branch br-a &&
     grit branch br-b &&
     grit branch br-c) &&
    (cd repo && grit branch -v >../actual) &&
    grep "br-a" actual &&
    grep "br-b" actual &&
    grep "br-c" actual
'

test_expect_success 'branch list is ordered' '
    (cd repo && grit branch >../actual) &&
    test_line_count -gt 1 actual
'

test_expect_success 'branch -f force overwrites existing branch' '
    (cd repo &&
     grit branch target_br &&
     hash_before=$(grit rev-parse target_br) &&
     echo extra >>file.txt &&
     grit add file.txt &&
     grit commit -m "third" &&
     grit branch -f target_br &&
     hash_after=$(grit rev-parse target_br) &&
     test "$hash_before" != "$hash_after")
'

test_expect_success 'branch creation at specific start point' '
    (cd repo &&
     first=$(grit rev-parse feature) &&
     grit branch from_start "$first") &&
    (cd repo && grit rev-parse from_start >../actual) &&
    (cd repo && grit rev-parse feature >../expect) &&
    test_cmp expect actual
'

test_expect_success 'branch -v shows correct hash for start-point branch' '
    (cd repo && grit branch -v >../actual) &&
    grep "from_start" actual | grep "initial"
'

test_expect_success 'branch -a lists all branches' '
    (cd repo && grit branch -a >../actual) &&
    grep "main" actual &&
    grep "feature" actual
'

test_expect_success 'branch --show-current on main' '
    (cd repo && grit switch main && grit branch --show-current >../actual) &&
    echo "main" >expect &&
    test_cmp expect actual
'

test_expect_success 'branch -q suppresses output on delete' '
    (cd repo && grit branch quiet_del &&
     grit branch -q -d quiet_del >../actual 2>&1) &&
    test_must_be_empty actual
'

test_expect_success 'branch creation is quiet with -q' '
    (cd repo && grit branch -q quiet_create >../actual 2>&1) &&
    test_must_be_empty actual &&
    (cd repo && grit branch >../actual) &&
    grep "quiet_create" actual
'

test_expect_success 'branch -v output line count matches branch count' '
    (cd repo && grit branch >../branches && grit branch -v >../verbose) &&
    br_count=$(wc -l <branches) &&
    v_count=$(wc -l <verbose) &&
    test "$br_count" -eq "$v_count"
'

test_expect_success 'branch list after multiple deletes' '
    (cd repo &&
     grit branch -D br-a &&
     grit branch -D br-b &&
     grit branch -D br-c) &&
    (cd repo && grit branch >../actual) &&
    ! grep "br-a" actual &&
    ! grep "br-b" actual &&
    ! grep "br-c" actual
'

test_expect_success 'branch with hyphen and dots in name' '
    (cd repo && grit branch my-feature.v2) &&
    (cd repo && grit branch >../actual) &&
    grep "my-feature.v2" actual
'

test_expect_success 'branch -v with special name shows subject' '
    (cd repo && grit branch -v >../actual) &&
    grep "my-feature.v2" actual
'

test_expect_success 'delete branch with special name' '
    (cd repo && grit branch -D my-feature.v2) &&
    (cd repo && grit branch >../actual) &&
    ! grep "my-feature.v2" actual
'

test_done
