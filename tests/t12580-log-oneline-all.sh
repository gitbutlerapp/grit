#!/bin/sh

test_description='grit log --oneline with various options and multiple branches'

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
    git reset --hard HEAD~1 &&
    git checkout side &&
    echo four >file4.txt && grit add file4.txt && grit commit -m "side-one" &&
    echo five >file5.txt && grit add file5.txt && grit commit -m "side-two" &&
    git checkout main
    )
'

test_expect_success 'log --oneline shows abbreviated hash and subject' '
    (cd repo && grit log --oneline -n 1 >../actual) &&
    (cd repo && git log --oneline -n 1 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline shows all commits on current branch' '
    (cd repo && grit log --oneline >../actual) &&
    (cd repo && git log --oneline >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline -n limits output' '
    (cd repo && grit log --oneline -n 2 >../actual) &&
    (cd repo && git log --oneline -n 2 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline -n 1 shows only tip' '
    (cd repo && grit log --oneline -n 1 >../actual) &&
    wc -l <actual >line_count &&
    echo 1 >expect_count &&
    test_cmp expect_count line_count
'

test_expect_success 'log --oneline --reverse shows oldest first' '
    (cd repo && grit log --oneline --reverse >../actual) &&
    (cd repo && git log --oneline --reverse >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline --reverse first line is root commit' '
    (cd repo && grit log --oneline --reverse >../actual) &&
    head -1 actual >first_line &&
    grep "first" first_line
'

test_expect_success 'log --oneline --skip=1 skips the latest commit' '
    (cd repo && grit log --oneline --skip=1 >../actual) &&
    (cd repo && git log --oneline --skip=1 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline --skip=2 skips two commits' '
    (cd repo && grit log --oneline --skip=2 >../actual) &&
    (cd repo && git log --oneline --skip=2 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline -n 1 --skip=1 shows second commit' '
    (cd repo && grit log --oneline -n 1 --skip=1 >../actual) &&
    (cd repo && git log --oneline -n 1 --skip=1 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline side branch shows side commits' '
    (cd repo && grit log --oneline side >../actual) &&
    (cd repo && git log --oneline side >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline main and side combined' '
    (cd repo && grit log --oneline main side >../actual) &&
    (cd repo && git log --oneline main side >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline --no-decorate strips decorations' '
    (cd repo && grit log --oneline --no-decorate >../actual) &&
    (cd repo && git log --oneline --no-decorate >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline --no-decorate has no parens' '
    (cd repo && grit log --oneline --no-decorate >../actual) &&
    ! grep "(" actual
'

test_expect_success 'log --oneline --decorate shows branch names' '
    (cd repo && grit log --oneline --decorate >../actual) &&
    grep "main" actual
'

test_expect_success 'log --oneline --decorate=short matches git' '
    (cd repo && grit log --oneline --decorate=short >../actual) &&
    (cd repo && git log --oneline --decorate=short >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline --decorate=full shows refs/heads' '
    (cd repo && grit log --oneline --decorate=full >../actual) &&
    (cd repo && git log --oneline --decorate=full >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline --graph matches git' '
    (cd repo && grit log --oneline --graph >../actual) &&
    (cd repo && git log --oneline --graph >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline --first-parent on main' '
    (cd repo && grit log --oneline --first-parent >../actual) &&
    (cd repo && git log --oneline --first-parent >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline explicit revision HEAD' '
    (cd repo && grit log --oneline HEAD >../actual) &&
    (cd repo && git log --oneline HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline combined -n and --reverse' '
    (cd repo && grit log --oneline -n 2 --reverse >../actual) &&
    (cd repo && git log --oneline -n 2 --reverse >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline combined --skip and -n' '
    (cd repo && grit log --oneline --skip=1 -n 1 >../actual) &&
    (cd repo && git log --oneline --skip=1 -n 1 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline --skip and -n combined on side' '
    (cd repo && grit log --oneline --skip=1 -n 2 side >../actual) &&
    (cd repo && git log --oneline --skip=1 -n 2 side >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline shows correct count on main' '
    (cd repo && grit log --oneline >../actual) &&
    wc -l <actual >count &&
    echo 2 >expect_count &&
    test_cmp expect_count count
'

test_expect_success 'log --oneline shows correct count on side' '
    (cd repo && grit log --oneline side >../actual) &&
    wc -l <actual >count &&
    echo 5 >expect_count &&
    test_cmp expect_count count
'

test_expect_success 'log --oneline --reverse --no-decorate matches git' '
    (cd repo && grit log --oneline --reverse --no-decorate >../actual) &&
    (cd repo && git log --oneline --reverse --no-decorate >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline --skip exceeding total shows nothing' '
    (cd repo && grit log --oneline --skip=100 >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'log --oneline -n 1 --no-decorate output is clean' '
    (cd repo && grit log --oneline -n 1 --no-decorate >../actual) &&
    (cd repo && git log --oneline -n 1 --no-decorate >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline --graph --no-decorate matches git' '
    (cd repo && grit log --oneline --graph --no-decorate >../actual) &&
    (cd repo && git log --oneline --graph --no-decorate >../expect) &&
    test_cmp expect actual
'

test_expect_success 'log --oneline hash is 7 chars' '
    (cd repo && grit log --oneline -n 1 --no-decorate >../actual) &&
    cut -d" " -f1 <actual >hash &&
    len=$(wc -c <hash) &&
    test "$len" -eq 8
'

test_expect_success 'log --oneline with --skip and --reverse' '
    (cd repo && grit log --oneline --skip=1 --reverse >../actual) &&
    (cd repo && git log --oneline --skip=1 --reverse >../expect) &&
    test_cmp expect actual
'

test_done
