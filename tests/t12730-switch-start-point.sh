#!/bin/sh
test_description='grit switch: -c, -C, --detach, --orphan, start-point, branch switching'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo first >file.txt &&
     grit add file.txt &&
     grit commit -m "first" &&
     grit rev-parse HEAD >../first_hash &&
     echo second >file.txt &&
     grit add file.txt &&
     grit commit -m "second" &&
     grit rev-parse HEAD >../second_hash &&
     echo third >file.txt &&
     grit add file.txt &&
     grit commit -m "third" &&
     grit rev-parse HEAD >../third_hash)
'

test_expect_success 'switch -c creates new branch at HEAD' '
    (cd repo &&
     grit switch -c feature1 &&
     grit rev-parse HEAD >../actual) &&
    test_cmp third_hash actual
'

test_expect_success 'new branch is current branch' '
    (cd repo &&
     grit symbolic-ref HEAD >../actual) &&
    echo "refs/heads/feature1" >expect &&
    test_cmp expect actual
'

test_expect_success 'switch back to main' '
    (cd repo && grit switch main)
'

test_expect_success 'switch -c with start-point creates branch at that commit' '
    (cd repo &&
     grit switch -c from-first "$(cat ../first_hash)" &&
     grit rev-parse HEAD >../actual) &&
    test_cmp first_hash actual
'

test_expect_success 'worktree reflects start-point content' '
    (cd repo && cat file.txt >../actual) &&
    echo "first" >expect &&
    test_cmp expect actual
'

test_expect_success 'switch back to main again' '
    (cd repo && grit switch main)
'

test_expect_success 'switch to existing branch' '
    (cd repo &&
     grit switch feature1 &&
     grit symbolic-ref HEAD >../actual) &&
    echo "refs/heads/feature1" >expect &&
    test_cmp expect actual
'

test_expect_success 'switch to main and verify content' '
    (cd repo &&
     grit switch main &&
     cat file.txt >../actual) &&
    echo "third" >expect &&
    test_cmp expect actual
'

test_expect_success 'switch --detach goes to detached HEAD' '
    (cd repo &&
     grit switch --detach main &&
     grit rev-parse HEAD >../actual) &&
    test_cmp third_hash actual
'

test_expect_success 'HEAD is detached (not a symbolic ref)' '
    (cd repo &&
     test_must_fail grit symbolic-ref HEAD 2>../err) &&
    test -s err
'

test_expect_success 'switch back from detached HEAD' '
    (cd repo && grit switch main)
'

test_expect_success 'switch --detach to specific commit' '
    (cd repo &&
     grit switch --detach "$(cat ../second_hash)" &&
     grit rev-parse HEAD >../actual) &&
    test_cmp second_hash actual
'

test_expect_success 'switch back to main from detached' '
    (cd repo && grit switch main)
'

test_expect_success 'switch --orphan creates branch with no history' '
    (cd repo &&
     grit switch --orphan empty-branch &&
     grit symbolic-ref HEAD >../actual) &&
    echo "refs/heads/empty-branch" >expect &&
    test_cmp expect actual
'

test_expect_success 'orphan branch has no commits yet' '
    (cd repo &&
     test_must_fail grit rev-parse HEAD 2>../err) &&
    test -s err
'

test_expect_success 'switch back to main from orphan' '
    (cd repo && grit switch main)
'

test_expect_success 'switch -c with tag-like ref as start-point' '
    (cd repo &&
     grit tag v1.0 "$(cat ../first_hash)" &&
     grit switch -c from-tag v1.0 &&
     grit rev-parse HEAD >../actual) &&
    test_cmp first_hash actual
'

test_expect_success 'switch back to main' '
    (cd repo && grit switch main)
'

test_expect_success 'switch -c to another start-point from non-main branch' '
    (cd repo &&
     grit switch from-first &&
     grit switch -c branched-from-first "$(cat ../second_hash)" &&
     grit rev-parse HEAD >../actual) &&
    test_cmp second_hash actual
'

test_expect_success 'switch back to main' '
    (cd repo && grit switch main)
'

test_expect_success 'switch --detach from a non-main branch' '
    (cd repo &&
     grit switch feature1 &&
     grit switch --detach "$(cat ../first_hash)" &&
     grit rev-parse HEAD >../actual) &&
    test_cmp first_hash actual
'

test_expect_success 'switch back to main from detached' '
    (cd repo && grit switch main)
'

test_expect_success 'switch -c fails if branch already exists' '
    (cd repo &&
     test_must_fail grit switch -c feature1 2>../err) &&
    test -s err
'

test_expect_success 'switch to nonexistent branch fails' '
    (cd repo &&
     test_must_fail grit switch nonexistent 2>../err) &&
    test -s err
'

test_expect_success 'switch -c from-second at second creates correct branch' '
    (cd repo &&
     grit switch -c from-second "$(cat ../second_hash)" &&
     cat file.txt >../actual) &&
    echo "second" >expect &&
    test_cmp expect actual
'

test_expect_success 'branch list shows all created branches' '
    (cd repo &&
     grit branch >../actual) &&
    grep "feature1" actual &&
    grep "from-first" actual &&
    grep "from-second" actual &&
    grep "from-tag" actual &&
    grep "main" actual
'

test_expect_success 'switch to main and make a new commit' '
    (cd repo &&
     grit switch main &&
     echo fourth >file.txt &&
     grit add file.txt &&
     grit commit -m "fourth" &&
     grit rev-parse HEAD >../fourth_hash)
'

test_expect_success 'switch -c from HEAD~1 works' '
    (cd repo &&
     grit switch -c from-parent HEAD~1 &&
     grit rev-parse HEAD >../actual) &&
    test_cmp third_hash actual
'

test_expect_success 'switch back to main' '
    (cd repo && grit switch main)
'

test_expect_success 'switch --detach HEAD~2 goes two back' '
    (cd repo &&
     grit switch --detach HEAD~2 &&
     grit rev-parse HEAD >../actual) &&
    test_cmp second_hash actual
'

test_expect_success 'switch to main' '
    (cd repo && grit switch main)
'

test_expect_success 'switch preserves uncommitted compatible changes' '
    (cd repo &&
     echo untracked >new-untracked.txt &&
     grit switch feature1 &&
     test -f new-untracked.txt) &&
    (cd repo && rm -f new-untracked.txt && grit switch main)
'

test_expect_success 'switch -c with abbreviated hash' '
    (cd repo &&
     short=$(cat ../first_hash | cut -c1-8) &&
     grit switch -c from-short "$short" &&
     grit rev-parse HEAD >../actual) &&
    test_cmp first_hash actual
'

test_expect_success 'cleanup - back to main' '
    (cd repo && grit switch main)
'

test_expect_success 'final: main HEAD is fourth commit' '
    (cd repo && grit rev-parse HEAD >../actual) &&
    test_cmp fourth_hash actual
'

test_done
