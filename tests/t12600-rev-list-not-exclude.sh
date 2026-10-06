#!/bin/sh

test_description='grit rev-list with caret exclusion, range notation, and filtering'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
    (
    grit init repo && cd repo &&
    git config user.email "t@t.com" && git config user.name "T" &&
    echo one >file.txt && grit add file.txt && grit commit -m "first" &&
    echo two >file2.txt && grit add file2.txt && grit commit -m "second" &&
    echo three >file3.txt && grit add file3.txt && grit commit -m "third" &&
    git branch side &&
    echo four >file4.txt && grit add file4.txt && grit commit -m "fourth" &&
    git checkout side &&
    echo five >side.txt && grit add side.txt && grit commit -m "side-one" &&
    echo six >side2.txt && grit add side2.txt && grit commit -m "side-two" &&
    git checkout main
    )
'

test_expect_success 'rev-list HEAD lists all commits' '
    (cd repo && grit rev-list HEAD >../actual) &&
    (cd repo && git rev-list HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list HEAD count matches git' '
    (cd repo && grit rev-list --count HEAD >../actual) &&
    echo 4 >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list side count' '
    (cd repo && grit rev-list --count side >../actual) &&
    echo 5 >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list ^ref excludes ancestors' '
    (cd repo && grit rev-list "^side" main >../actual) &&
    (cd repo && git rev-list "^side" main >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list ^ref exclusion shows only unique commits' '
    (cd repo && grit rev-list --count "^side" main >../actual) &&
    echo 1 >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list side..main is equivalent to ^side main' '
    (cd repo && grit rev-list side..main >../actual) &&
    (cd repo && grit rev-list "^side" main >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list main..side shows side-only commits' '
    (cd repo && grit rev-list main..side >../actual) &&
    (cd repo && git rev-list main..side >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list main..side count is 2' '
    (cd repo && grit rev-list --count main..side >../actual) &&
    echo 2 >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list side..main count is 1' '
    (cd repo && grit rev-list --count side..main >../actual) &&
    echo 1 >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list ^main side matches main..side' '
    (cd repo && grit rev-list "^main" side >../actual) &&
    (cd repo && grit rev-list main..side >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list same..same is empty' '
    (cd repo && grit rev-list main..main >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'rev-list --max-count=1 HEAD shows only tip' '
    (cd repo && grit rev-list --max-count=1 HEAD >../actual) &&
    (cd repo && git rev-list --max-count=1 HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --max-count=2 HEAD shows two' '
    (cd repo && grit rev-list --max-count=2 HEAD >../actual) &&
    (cd repo && git rev-list --max-count=2 HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --skip=1 HEAD skips tip' '
    (cd repo && grit rev-list --skip=1 HEAD >../actual) &&
    (cd repo && git rev-list --skip=1 HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --skip=2 HEAD' '
    (cd repo && grit rev-list --skip=2 HEAD >../actual) &&
    (cd repo && git rev-list --skip=2 HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --skip exceeding count gives empty' '
    (cd repo && grit rev-list --skip=100 HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'rev-list --reverse HEAD shows oldest first' '
    (cd repo && grit rev-list --reverse HEAD >../actual) &&
    (cd repo && git rev-list --reverse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --reverse first line is root commit' '
    (cd repo && grit rev-list --reverse HEAD >../actual) &&
    head -1 actual >first_hash &&
    (cd repo && grit rev-list --reverse HEAD >../expect_all) &&
    head -1 expect_all >expect_hash &&
    (cd repo && git rev-list --reverse HEAD >../git_all) &&
    head -1 git_all >git_hash &&
    test_cmp git_hash first_hash
'

test_expect_success 'rev-list --first-parent HEAD' '
    (cd repo && grit rev-list --first-parent HEAD >../actual) &&
    (cd repo && git rev-list --first-parent HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list multiple refs lists union' '
    (cd repo && grit rev-list main side >../actual) &&
    (cd repo && git rev-list main side >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list multiple refs count' '
    (cd repo && grit rev-list --count main side >../actual) &&
    echo 6 >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list --all lists all reachable commits' '
    (cd repo && grit rev-list --all >../actual) &&
    (cd repo && git rev-list --all >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --all --count' '
    (cd repo && grit rev-list --count --all >../actual) &&
    echo 6 >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list --max-count=1 --skip=1 HEAD' '
    (cd repo && grit rev-list --max-count=1 --skip=1 HEAD >../actual) &&
    (cd repo && git rev-list --max-count=1 --skip=1 HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --reverse --max-count=2 HEAD' '
    (cd repo && grit rev-list --reverse --max-count=2 HEAD >../actual) &&
    (cd repo && git rev-list --reverse --max-count=2 HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list output is 40-char hex hashes' '
    (cd repo && grit rev-list -n 1 HEAD >../actual) &&
    len=$(wc -c <actual) &&
    test "$len" -eq 41
'

test_expect_success 'rev-list ^third_commit shows root only commits before it' '
    (cd repo && third=$(git rev-parse HEAD~1) &&
    grit rev-list "^$third" HEAD >../actual) &&
    wc -l <actual >count &&
    echo 1 >expect_count &&
    test_cmp expect_count count
'

test_expect_success 'rev-list range with raw hashes' '
    (cd repo &&
    parent=$(git rev-parse HEAD~1) &&
    grit rev-list "$parent..HEAD" >../actual) &&
    (cd repo &&
    parent=$(git rev-parse HEAD~1) &&
    git rev-list "$parent..HEAD" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list empty range (ancestor..ancestor)' '
    (cd repo &&
    root=$(git rev-list --reverse HEAD | head -1) &&
    grit rev-list "$root..$root" >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'rev-list --count with range' '
    (cd repo && grit rev-list --count side..main >../actual) &&
    (cd repo && git rev-list --count side..main >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --reverse with range' '
    (cd repo && grit rev-list --reverse main..side >../actual) &&
    (cd repo && git rev-list --reverse main..side >../expect) &&
    test_cmp expect actual
'

test_done
