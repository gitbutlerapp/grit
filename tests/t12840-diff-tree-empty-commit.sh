#!/bin/sh

test_description='diff-tree: empty commits, root commits, single vs two-tree comparisons, various output formats'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with initial commit' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo hello >file.txt &&
     grit add file.txt &&
     grit commit -m "initial"
    )
'

test_expect_success 'diff-tree -r on root commit produces no output' '
    (cd repo && grit diff-tree -r HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'diff-tree -p on root commit produces no output' '
    (cd repo && grit diff-tree -p HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'create empty commit' '
    (cd repo && grit commit --allow-empty -m "empty commit")
'

test_expect_success 'diff-tree -r between parent and empty commit is empty' '
    (cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'diff-tree -p between parent and empty commit is empty' '
    (cd repo && grit diff-tree -p HEAD~1 HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'diff-tree --name-only for empty commit is empty' '
    (cd repo && grit diff-tree --name-only HEAD~1 HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'diff-tree --name-status for empty commit is empty' '
    (cd repo && grit diff-tree --name-status HEAD~1 HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'diff-tree --stat for empty commit shows 0 files' '
    (cd repo && grit diff-tree --stat HEAD~1 HEAD >../actual) &&
    grep "0 files changed" actual
'

test_expect_success 'add file after empty commit' '
    (cd repo &&
     echo new >new.txt &&
     grit add new.txt &&
     grit commit -m "add new after empty"
    )
'

test_expect_success 'diff-tree -r skipping empty commit shows addition' '
    (cd repo && grit diff-tree -r HEAD~2 HEAD >../actual) &&
    grep "A" actual &&
    grep "new.txt" actual
'

test_expect_success 'diff-tree -r from empty commit to next shows same addition' '
    (cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
    grep "A" actual &&
    grep "new.txt" actual
'

test_expect_success 'diff-tree --name-status shows A for addition' '
    (cd repo && grit diff-tree --name-status HEAD~1 HEAD >../actual) &&
    grep "^A" actual &&
    grep "new.txt" actual
'

test_expect_success 'diff-tree --name-only shows just filename' '
    (cd repo && grit diff-tree --name-only HEAD~1 HEAD >../actual) &&
    echo "new.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'multiple empty commits in a row' '
    (cd repo &&
     grit commit --allow-empty -m "empty2" &&
     grit commit --allow-empty -m "empty3" &&
     grit commit --allow-empty -m "empty4"
    )
'

test_expect_success 'diff-tree across multiple empty commits is empty' '
    (cd repo && grit diff-tree -r HEAD~3 HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'diff-tree from add-new commit to last empty shows no changes' '
    (cd repo && grit diff-tree -r HEAD~3 HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'diff-tree from initial to after empties shows addition' '
    (cd repo && grit diff-tree -r HEAD~5 HEAD >../actual) &&
    grep "new.txt" actual
'

test_expect_success 'add file after series of empty commits' '
    (cd repo &&
     echo after >after.txt &&
     grit add after.txt &&
     grit commit -m "add after empties"
    )
'

test_expect_success 'diff-tree from last empty to new commit shows addition' '
    (cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
    grep "A" actual &&
    grep "after.txt" actual
'

test_expect_success 'diff-tree -p from empty to next shows patch' '
    (cd repo && grit diff-tree -p HEAD~1 HEAD >../actual) &&
    grep "diff --git" actual &&
    grep "after.txt" actual &&
    grep "+after" actual
'

test_expect_success 'diff-tree --stat shows 1 file changed' '
    (cd repo && grit diff-tree --stat HEAD~1 HEAD >../actual) &&
    grep "1 file changed" actual
'

test_expect_success 'setup: delete file then empty commit' '
    (cd repo &&
     git rm file.txt &&
     grit commit -m "delete file" &&
     grit commit --allow-empty -m "empty after delete"
    )
'

test_expect_success 'diff-tree shows deletion before empty' '
    (cd repo && grit diff-tree -r HEAD~2 HEAD~1 >../actual) &&
    grep "D" actual &&
    grep "file.txt" actual
'

test_expect_success 'diff-tree across empty after delete still shows delete' '
    (cd repo && grit diff-tree -r HEAD~2 HEAD >../actual) &&
    grep "D" actual &&
    grep "file.txt" actual
'

test_expect_success 'setup: modify file then empty commit' '
    (cd repo &&
     echo modified >new.txt &&
     grit add new.txt &&
     grit commit -m "modify new.txt" &&
     grit commit --allow-empty -m "empty after modify"
    )
'

test_expect_success 'diff-tree shows modification before empty' '
    (cd repo && grit diff-tree -r HEAD~2 HEAD~1 >../actual) &&
    grep "M" actual &&
    grep "new.txt" actual
'

test_expect_success 'diff-tree across empty shows same modification' '
    (cd repo && grit diff-tree -r HEAD~2 HEAD >../actual) &&
    grep "M" actual &&
    grep "new.txt" actual
'

test_expect_success 'empty commit between two real changes' '
    (cd repo &&
     echo v1 >changing.txt &&
     grit add changing.txt &&
     grit commit -m "add changing" &&
     grit commit --allow-empty -m "gap" &&
     echo v2 >changing.txt &&
     grit add changing.txt &&
     grit commit -m "update changing"
    )
'

test_expect_success 'diff-tree across the gap shows M for changing.txt' '
    (cd repo && grit diff-tree -r HEAD~2 HEAD >../actual) &&
    grep "M" actual &&
    grep "changing.txt" actual
'

test_expect_success 'diff-tree first-change to gap is empty' '
    (cd repo && grit diff-tree -r HEAD~2 HEAD~1 >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'diff-tree gap to second-change shows M' '
    (cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
    grep "M" actual &&
    grep "changing.txt" actual
'

test_expect_success 'diff-tree -p for empty commit pair is empty' '
    (cd repo && grit diff-tree -p HEAD~2 HEAD~1 >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'diff-tree --stat for non-empty change shows stats' '
    (cd repo && grit diff-tree --stat HEAD~1 HEAD >../actual) &&
    grep "changing.txt" actual &&
    grep "1 file changed" actual
'

test_expect_success 'diff-tree same commit to itself is empty' '
    (cd repo && grit diff-tree -r HEAD HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'diff-tree -r with two identical trees produces no output' '
    (cd repo &&
     grit commit --allow-empty -m "another empty"
    ) &&
    (cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
    test_must_be_empty actual
'

test_done
