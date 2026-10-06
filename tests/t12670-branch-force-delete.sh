#!/bin/sh

test_description='branch force-delete (-D), safe-delete (-d), rename (-m/-M), list, verbose, --show-current, --contains, --merged'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo hello >file.txt &&
     grit add file.txt &&
     grit commit -m "initial" &&
     echo second >file.txt &&
     grit add file.txt &&
     grit commit -m "second" &&
     grit branch side &&
     grit branch alt &&
     grit branch feature/login
    )
'

test_expect_success 'branch -l lists branches' '
    (cd repo && grit branch -l >../actual) &&
    grep "main" actual &&
    grep "side" actual &&
    grep "alt" actual &&
    grep "feature/login" actual
'

test_expect_success 'current branch has asterisk' '
    (cd repo && grit branch -l >../actual) &&
    grep "^\* main" actual
'

test_expect_success 'branch --show-current shows main' '
    (cd repo && grit branch --show-current >../actual) &&
    echo "main" >expect &&
    test_cmp expect actual
'

test_expect_success 'branch -v shows commit subject' '
    (cd repo && grit branch -v >../actual) &&
    grep "second" actual
'

test_expect_success 'branch -d deletes a branch' '
    (cd repo && grit branch -d side >../actual 2>&1) &&
    grep "Deleted branch side" actual
'

test_expect_success 'deleted branch no longer in list' '
    (cd repo && grit branch -l >../actual) &&
    ! grep "side" actual
'

test_expect_success 'branch -d nonexistent fails' '
    (cd repo && ! grit branch -d nonexistent 2>../actual_err) &&
    grep "not found" actual_err
'

test_expect_success 'branch -D nonexistent fails' '
    (cd repo && ! grit branch -D nonexistent 2>../actual_err) &&
    grep "not found" actual_err
'

test_expect_success 'branch -d cannot delete current branch' '
    (cd repo && ! grit branch -d main 2>../actual_err) &&
    grep -i "cannot delete" actual_err
'

test_expect_success 'branch -D cannot delete current branch' '
    (cd repo && ! grit branch -D main 2>../actual_err) &&
    grep -i "cannot delete" actual_err
'

test_expect_success 'branch -D deletes any branch' '
    (cd repo && grit branch -D alt >../actual 2>&1) &&
    grep "Deleted branch alt" actual
'

test_expect_success 'setup diverged branch for force-delete' '
    (cd repo &&
     grit branch diverged &&
     grit switch diverged &&
     echo diverge >new.txt &&
     grit add new.txt &&
     grit commit -m "diverge" &&
     grit switch main
    )
'

test_expect_success 'branch -D force-deletes diverged branch' '
    (cd repo && grit branch -D diverged >../actual 2>&1) &&
    grep "Deleted branch diverged" actual
'

test_expect_success 'branch -D output includes short SHA' '
    (cd repo &&
     grit branch todelete &&
     grit branch -D todelete >../actual 2>&1) &&
    grep "(was [0-9a-f]" actual
'

test_expect_success 'branch -m renames a branch' '
    (cd repo &&
     grit branch old-name &&
     grit branch -m old-name new-name &&
     grit branch -l >../actual) &&
    grep "new-name" actual &&
    ! grep "old-name" actual
'

test_expect_success 'branch -m to same name is a no-op' '
    (cd repo && grit branch -m main)
'

test_expect_success 'branch -m fails if target exists' '
    (cd repo &&
     grit branch exists-target &&
     ! grit branch -m new-name exists-target 2>../actual_err) &&
    grep "already exists" actual_err
'

test_expect_success 'branch -M force-renames over existing' '
    (cd repo && grit branch -M new-name exists-target) &&
    (cd repo && grit branch -l >../actual) &&
    grep "exists-target" actual &&
    ! grep "new-name" actual
'

test_expect_success 'create branch at specific start point' '
    (cd repo &&
     parent=$(grit rev-parse HEAD^) &&
     grit branch from-point $parent &&
     grit log --format="%s" -n 1 from-point >../actual) &&
    echo "initial" >expect &&
    test_cmp expect actual
'

test_expect_success 'branch -f overrides existing branch' '
    (cd repo &&
     grit branch -f from-point HEAD &&
     grit log --format="%s" -n 1 from-point >../actual) &&
    echo "second" >expect &&
    test_cmp expect actual
'

test_expect_success 'branch without -f fails on existing' '
    (cd repo && ! grit branch from-point HEAD 2>../actual_err) &&
    grep "already exists" actual_err
'

test_expect_success 'branch --contains shows containing branches' '
    (cd repo && grit branch --contains HEAD >../actual) &&
    grep "main" actual
'

test_expect_success 'branch --merged shows merged branches' '
    (cd repo && grit branch --merged main >../actual) &&
    grep "main" actual
'

test_expect_success 'branch --show-current after switch' '
    (cd repo &&
     grit switch exists-target &&
     grit branch --show-current >../actual &&
     grit switch main) &&
    echo "exists-target" >expect &&
    test_cmp expect actual
'

test_expect_success 'delete multiple branches sequentially' '
    (cd repo &&
     grit branch del1 &&
     grit branch del2 &&
     grit branch del3 &&
     grit branch -d del1 &&
     grit branch -d del2 &&
     grit branch -d del3 &&
     grit branch -l >../actual) &&
    ! grep "del1" actual &&
    ! grep "del2" actual &&
    ! grep "del3" actual
'

test_expect_success 'branch with slash in name' '
    (cd repo && grit branch -l >../actual) &&
    grep "feature/login" actual
'

test_expect_success 'delete branch with slash' '
    (cd repo && grit branch -d feature/login >../actual 2>&1) &&
    grep "Deleted" actual
'

test_expect_success 'create branch with dots in name' '
    (cd repo && grit branch release-1.0.0 &&
     grit branch -l >../actual) &&
    grep "release-1.0.0" actual
'

test_expect_success 'branch -d the dotted branch' '
    (cd repo && grit branch -d release-1.0.0 >../actual 2>&1) &&
    grep "Deleted branch release-1.0.0" actual
'

test_expect_success 'branch -q suppresses output on delete' '
    (cd repo &&
     grit branch quiet-del &&
     grit branch -d -q quiet-del >../actual 2>&1) &&
    test_must_be_empty actual
'

test_expect_success 'branch list after all deletions is clean' '
    (cd repo && grit branch -l >../actual) &&
    grep "main" actual &&
    grep "exists-target" actual &&
    grep "from-point" actual
'

test_done
