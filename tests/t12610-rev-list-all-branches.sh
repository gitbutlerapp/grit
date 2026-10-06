#!/bin/sh

test_description='grit rev-list --all with multiple branches and various topologies'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup linear history' '
    grit init repo && cd repo &&
    git config user.email "t@t.com" && git config user.name "T" &&
    echo one >file.txt && grit add file.txt && grit commit -m "c1" &&
    echo two >file2.txt && grit add file2.txt && grit commit -m "c2" &&
    echo three >file3.txt && grit add file3.txt && grit commit -m "c3" &&
    cd ..
'

test_expect_success 'rev-list --all on single branch' '
    (cd repo && grit rev-list --all >../actual) &&
    (cd repo && git rev-list --all >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --all --count on single branch' '
    (cd repo && grit rev-list --count --all >../actual) &&
    echo 3 >expect &&
    test_cmp expect actual
'

test_expect_success 'setup second branch with divergent history' '
    (cd repo && git branch branchA &&
    git checkout branchA &&
    echo a1 >a1.txt && grit add a1.txt && grit commit -m "a1" &&
    echo a2 >a2.txt && grit add a2.txt && grit commit -m "a2" &&
    git checkout main)
'

test_expect_success 'rev-list --all includes both branches' '
    (cd repo && grit rev-list --all >../actual) &&
    (cd repo && git rev-list --all >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --all --count with two branches' '
    (cd repo && grit rev-list --count --all >../actual) &&
    echo 5 >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list --all contains branchA commits' '
    (cd repo && grit rev-list --all >../all_commits &&
    branchA_tip=$(git rev-parse branchA) &&
    grep "$branchA_tip" ../all_commits)
'

test_expect_success 'rev-list --all contains main commits' '
    (cd repo && grit rev-list --all >../all_commits &&
    main_tip=$(git rev-parse main) &&
    grep "$main_tip" ../all_commits)
'

test_expect_success 'setup third branch' '
    (cd repo && git branch branchB &&
    git checkout branchB &&
    echo b1 >b1.txt && grit add b1.txt && grit commit -m "b1" &&
    git checkout main)
'

test_expect_success 'rev-list --all with three branches' '
    (cd repo && grit rev-list --all >../actual) &&
    (cd repo && git rev-list --all >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --all --count with three branches' '
    (cd repo && grit rev-list --count --all >../actual) &&
    echo 6 >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list --all --reverse' '
    (cd repo && grit rev-list --all --reverse >../actual) &&
    (cd repo && git rev-list --all --reverse >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --all --reverse first is root' '
    (cd repo && grit rev-list --all --reverse >../actual) &&
    head -1 actual >first &&
    (cd repo && git rev-list --all --reverse >../expect_all) &&
    head -1 expect_all >expect_root &&
    test_cmp expect_root first
'

test_expect_success 'rev-list --all --max-count=1' '
    (cd repo && grit rev-list --all --max-count=1 >../actual) &&
    wc -l <actual >count &&
    echo 1 >expect_count &&
    test_cmp expect_count count
'

test_expect_success 'rev-list --all --max-count=3' '
    (cd repo && grit rev-list --all --max-count=3 >../actual) &&
    (cd repo && git rev-list --all --max-count=3 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --all --skip=2' '
    (cd repo && grit rev-list --all --skip=2 >../actual) &&
    (cd repo && git rev-list --all --skip=2 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --all --skip=2 --max-count=2' '
    (cd repo && grit rev-list --all --skip=2 --max-count=2 >../actual) &&
    (cd repo && git rev-list --all --skip=2 --max-count=2 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list main vs rev-list --all differs' '
    (cd repo && grit rev-list main >../main_only) &&
    (cd repo && grit rev-list --all >../all_commits) &&
    ! test_cmp main_only all_commits
'

test_expect_success 'rev-list branchA vs --all differs' '
    (cd repo && grit rev-list branchA >../branchA_only) &&
    (cd repo && grit rev-list --all >../all_commits) &&
    ! test_cmp branchA_only all_commits
'

test_expect_success 'rev-list --all deduplicates shared commits' '
    (cd repo && grit rev-list --all >../actual) &&
    sort actual >sorted &&
    sort -u actual >sorted_uniq &&
    test_cmp sorted_uniq sorted
'

test_expect_success 'rev-list main branchA union matches rev-list with both' '
    (cd repo && grit rev-list main branchA >../actual) &&
    (cd repo && git rev-list main branchA >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --all --first-parent' '
    (cd repo && grit rev-list --all --first-parent >../actual) &&
    (cd repo && git rev-list --all --first-parent >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list branch range with --all context' '
    (cd repo && grit rev-list main..branchA >../actual) &&
    (cd repo && git rev-list main..branchA >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list branch range count main..branchA' '
    (cd repo && grit rev-list --count main..branchA >../actual) &&
    echo 2 >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list branch range count main..branchB' '
    (cd repo && grit rev-list --count main..branchB >../actual) &&
    echo 1 >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list --all output is valid hex hashes' '
    (cd repo && grit rev-list --all >../actual) &&
    while read hash; do
        echo "$hash" | grep -q "^[0-9a-f]\{40\}$" || exit 1
    done <actual
'

test_expect_success 'rev-list --all matches rev-list of all branch names combined' '
    (cd repo && grit rev-list --all >../actual &&
    grit rev-list main branchA branchB >../combined) &&
    sort actual >sorted_all &&
    sort combined >sorted_combined &&
    test_cmp sorted_combined sorted_all
'

test_expect_success 'setup additional commit on main' '
    (cd repo && git checkout main &&
    echo extra >extra.txt && grit add extra.txt && grit commit -m "c4")
'

test_expect_success 'rev-list --all reflects new commit' '
    (cd repo && grit rev-list --count --all >../actual) &&
    echo 7 >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list --all --reverse --max-count=3' '
    (cd repo && grit rev-list --all --reverse --max-count=3 >../actual) &&
    (cd repo && git rev-list --all --reverse --max-count=3 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-list --count --all matches line count' '
    (cd repo && grit rev-list --all >../all_lines &&
    grit rev-list --count --all >../count_val) &&
    wc -l <all_lines >line_count &&
    test_cmp count_val line_count
'

test_expect_success 'rev-list --all --skip exceeding total is empty' '
    (cd repo && grit rev-list --all --skip=100 >../actual) &&
    test_must_be_empty actual
'

test_done
