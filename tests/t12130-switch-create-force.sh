#!/bin/sh
test_description='grit switch -c, --detach, --orphan, branch switching'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

grit_status () {
    grit status --porcelain | grep -v "^##" || true
}

current_branch () {
    git symbolic-ref --short HEAD 2>/dev/null
}

test_expect_success 'setup' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo hello >file.txt &&
     grit add file.txt &&
     grit commit -m "initial" &&
     echo second >second.txt &&
     grit add second.txt &&
     grit commit -m "second commit")
'

test_expect_success 'switch -c creates new branch' '
    (cd repo &&
     grit switch -c feature1) &&
    (cd repo && current_branch >../actual) &&
    echo "feature1" >expect &&
    test_cmp expect actual
'

test_expect_success 'new branch has same commits as source' '
    (cd repo &&
     grit log --oneline >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'switch back to main' '
    (cd repo &&
     grit switch main) &&
    (cd repo && current_branch >../actual) &&
    echo "main" >expect &&
    test_cmp expect actual
'

test_expect_success 'switch -c from specific start point' '
    (cd repo &&
     grit switch -c from-first HEAD~1) &&
    (cd repo && current_branch >../actual) &&
    echo "from-first" >expect &&
    test_cmp expect actual &&
    (cd repo && grit log --oneline >../actual) &&
    test_line_count = 1 actual
'

test_expect_success 'switch main again' '
    (cd repo && grit switch main)
'

test_expect_success 'switch -c fails if branch already exists' '
    (cd repo &&
     test_must_fail grit switch -c feature1 2>../actual) &&
    test_path_is_file actual
'

test_expect_success 'switch to existing branch by name' '
    (cd repo &&
     grit switch feature1) &&
    (cd repo && current_branch >../actual) &&
    echo "feature1" >expect &&
    test_cmp expect actual
'

test_expect_success 'switch main for detach tests' '
    (cd repo && grit switch main)
'

test_expect_success 'switch --detach goes to detached HEAD' '
    (cd repo &&
     grit switch --detach HEAD) &&
    (cd repo &&
     ! git symbolic-ref HEAD 2>/dev/null)
'

test_expect_success 'switch back to main from detached' '
    (cd repo && grit switch main)
'

test_expect_success 'switch --detach with commit ref' '
    (cd repo &&
     grit switch --detach HEAD~1) &&
    (cd repo && grit log --oneline >../actual) &&
    test_line_count = 1 actual
'

test_expect_success 'switch main after detach' '
    (cd repo && grit switch main)
'

test_expect_success 'switch preserves clean working tree across branches' '
    (cd repo &&
     grit switch -c clean-test &&
     echo new >new.txt &&
     grit add new.txt &&
     grit commit -m "add new" &&
     grit switch main &&
     test_path_is_missing new.txt &&
     grit switch clean-test &&
     test_path_is_file new.txt)
'

test_expect_success 'switch back to main' '
    (cd repo && grit switch main)
'

test_expect_success 'setup divergent branch for dirty-switch test' '
    (cd repo &&
     grit switch -c divergent &&
     echo different >file.txt &&
     grit add file.txt &&
     grit commit -m "diverge file.txt" &&
     grit switch main)
'

test_expect_success 'switch fails with conflicting dirty tracked file' '
    (cd repo &&
     echo dirty >file.txt &&
     test_must_fail grit switch divergent 2>../actual;
     grit restore file.txt)
'

test_expect_success 'verify working tree is clean after restore' '
    (cd repo && grit_status >../actual) &&
    test ! -s actual
'

test_expect_success 'switch -c creates branch and stays on it' '
    (cd repo &&
     grit switch -c stay-test &&
     echo content >stay.txt &&
     grit add stay.txt &&
     grit commit -m "stay commit") &&
    (cd repo && current_branch >../actual) &&
    echo "stay-test" >expect &&
    test_cmp expect actual
'

test_expect_success 'switch main for orphan test' '
    (cd repo && grit switch main)
'

test_expect_success 'switch --orphan creates parentless branch' '
    (cd repo &&
     grit switch --orphan orphan-branch) &&
    (cd repo && current_branch >../actual) &&
    echo "orphan-branch" >expect &&
    test_cmp expect actual
'

test_expect_success 'orphan branch has no commits' '
    (cd repo &&
	 test_must_fail grit rev-parse HEAD 2>../actual) &&
	test -s actual
'

test_expect_success 'can commit on orphan branch' '
    (cd repo &&
     echo orphan >orphan.txt &&
     grit add orphan.txt &&
     grit commit -m "orphan commit" &&
     grit log --oneline >../actual) &&
    test_line_count = 1 actual
'

test_expect_success 'switch back to main from orphan' '
    (cd repo && grit switch main) &&
    (cd repo && current_branch >../actual) &&
    echo "main" >expect &&
    test_cmp expect actual
'

test_expect_success 'switch to branch with different content' '
    (cd repo &&
     grit switch -c content-branch &&
     echo branch-content >branch-file.txt &&
     grit add branch-file.txt &&
     grit commit -m "branch content" &&
     grit switch main) &&
    test_path_is_missing repo/branch-file.txt &&
    (cd repo && grit switch content-branch) &&
    test_path_is_file repo/branch-file.txt
'

test_expect_success 'switch main for listing' '
    (cd repo && grit switch main)
'

test_expect_success 'multiple branches exist after creation' '
    (cd repo && grit branch >../actual) &&
    grep "main" actual &&
    grep "feature1" actual &&
    grep "clean-test" actual
'

test_expect_success 'switch to nonexistent branch fails' '
    (cd repo &&
     test_must_fail grit switch no-such-branch 2>../actual) &&
    test_path_is_file actual
'

test_expect_success 'switch -c with untracked file succeeds' '
    (cd repo &&
     echo untracked >untracked.txt &&
     grit switch -c untracked-test &&
     test_path_is_file untracked.txt) &&
    (cd repo && current_branch >../actual) &&
    echo "untracked-test" >expect &&
    test_cmp expect actual
'

test_expect_success 'switch main cleanup' '
    (cd repo &&
     rm -f untracked.txt &&
     grit switch main)
'

test_expect_success 'switch back and verify main still has both commits' '
    (cd repo &&
     grit log --oneline >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'switch --detach preserves working tree files' '
    (cd repo &&
     grit switch --detach HEAD &&
     test_path_is_file file.txt &&
     test_path_is_file second.txt)
'

test_expect_success 'final switch to main' '
    (cd repo && grit switch main) &&
    (cd repo && current_branch >../actual) &&
    echo "main" >expect &&
    test_cmp expect actual
'

test_done
