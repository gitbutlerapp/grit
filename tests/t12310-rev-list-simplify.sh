#!/bin/sh
test_description='rev-list ordering, filtering, and simplification with complex history'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup complex history with multiple branches and merges' '
    (
    grit init repo && cd repo &&
    git config user.email "t@t.com" &&
    git config user.name "T" &&
    echo base >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700000000 +0000" GIT_COMMITTER_DATE="1700000000 +0000" \
    grit commit -m "A-base" &&
    /usr/bin/git checkout -b branch1 &&
    echo b1-1 >b1.txt &&
    grit add b1.txt &&
    GIT_AUTHOR_DATE="1700001000 +0000" GIT_COMMITTER_DATE="1700001000 +0000" \
    grit commit -m "B1-first" &&
    echo b1-2 >b1-2.txt &&
    grit add b1-2.txt &&
    GIT_AUTHOR_DATE="1700002000 +0000" GIT_COMMITTER_DATE="1700002000 +0000" \
    grit commit -m "B1-second" &&
    /usr/bin/git checkout main &&
    /usr/bin/git checkout -b branch2 &&
    echo b2-1 >b2.txt &&
    grit add b2.txt &&
    GIT_AUTHOR_DATE="1700003000 +0000" GIT_COMMITTER_DATE="1700003000 +0000" \
    grit commit -m "B2-first" &&
    /usr/bin/git checkout main &&
    echo main1 >main1.txt &&
    grit add main1.txt &&
    GIT_AUTHOR_DATE="1700004000 +0000" GIT_COMMITTER_DATE="1700004000 +0000" \
    grit commit -m "M-first" &&
    /usr/bin/git merge branch1 -m "merge-branch1" &&
    /usr/bin/git merge branch2 -m "merge-branch2" &&
    echo final >final.txt &&
    grit add final.txt &&
    GIT_AUTHOR_DATE="1700007000 +0000" GIT_COMMITTER_DATE="1700007000 +0000" \
    grit commit -m "M-final"
    )
'

test_expect_success 'rev-list HEAD lists all commits' '
    (cd repo && grit rev-list HEAD >../actual) &&
    test_line_count = 8 actual
'

test_expect_success 'rev-list --count matches line count' '
    (cd repo && grit rev-list HEAD >../list &&
    grit rev-list --count HEAD >../count) &&
    LINES=$(wc -l <list) &&
    COUNT=$(cat count) &&
    test "$LINES" = "$COUNT"
'

test_expect_success 'rev-list --topo-order: merge parents appear before merge' '
    (cd repo && grit rev-list --topo-order HEAD >../actual) &&
    HEAD=$(cd repo && grit rev-parse HEAD) &&
    FIRST=$(head -1 actual) &&
    # HEAD (M-final) is first since its a non-merge descendant
    test "$FIRST" = "$HEAD"
'

test_expect_success 'rev-list --topo-order same count as default' '
    (cd repo && grit rev-list HEAD >../default_list &&
    grit rev-list --topo-order HEAD >../topo_list) &&
    test_line_count = 8 default_list &&
    test_line_count = 8 topo_list
'

test_expect_success 'rev-list --first-parent skips side branches' '
    (cd repo && grit rev-list --first-parent HEAD >../actual) &&
    # first-parent: M-final, merge-branch2, merge-branch1, M-first, A-base
    test_line_count = 5 actual
'

test_expect_success 'rev-list --first-parent does not contain branch1 commits' '
    (cd repo && grit rev-list --first-parent HEAD >../fp &&
    /usr/bin/git rev-parse branch1 >../b1_tip) &&
    B1=$(cat b1_tip) &&
    ! grep "$B1" fp
'

test_expect_success 'rev-list --first-parent does not contain branch2 commits' '
    (cd repo && grit rev-list --first-parent HEAD >../fp &&
    /usr/bin/git rev-parse branch2 >../b2_tip) &&
    B2=$(cat b2_tip) &&
    ! grep "$B2" fp
'

test_expect_success 'rev-list branch1 shows 3 commits' '
    (cd repo && grit rev-list branch1 >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'rev-list branch2 shows 2 commits' '
    (cd repo && grit rev-list branch2 >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'rev-list ^branch1 excludes branch1 reachable commits' '
    (cd repo && B1=$(/usr/bin/git rev-parse branch1) &&
    grit rev-list HEAD ^"$B1" >../actual) &&
    # Excluded: A-base, B1-first, B1-second
    # Remaining: M-first, merge-branch1, B2-first, merge-branch2, M-final
    test_line_count = 5 actual
'

test_expect_success 'rev-list ^branch2 excludes branch2 reachable commits' '
    (cd repo && B2=$(/usr/bin/git rev-parse branch2) &&
    grit rev-list HEAD ^"$B2" >../actual) &&
    # Excluded: A-base, B2-first
    # Remaining: B1-first, B1-second, M-first, merge-branch1, merge-branch2, M-final
    test_line_count = 6 actual
'

test_expect_success 'rev-list --reverse starts with root' '
    (cd repo && grit rev-list --reverse HEAD >../actual) &&
    ROOT=$(cd repo && grit rev-list --reverse HEAD | head -1) &&
    FIRST=$(head -1 actual) &&
    test "$FIRST" = "$ROOT"
'

test_expect_success 'rev-list --reverse is reverse of default' '
    (cd repo && grit rev-list HEAD >../forward &&
    grit rev-list --reverse HEAD >../reversed) &&
    tac forward >forward_rev &&
    test_cmp forward_rev reversed
'

test_expect_success 'rev-list --reverse --first-parent is reverse of --first-parent' '
    (cd repo && grit rev-list --first-parent HEAD >../fp_fwd &&
    grit rev-list --reverse --first-parent HEAD >../fp_rev) &&
    tac fp_fwd >fp_fwd_rev &&
    test_cmp fp_fwd_rev fp_rev
'

test_expect_success 'rev-list --max-count=1 returns exactly 1 commit' '
    (cd repo && grit rev-list --max-count=1 HEAD >../actual) &&
    test_line_count = 1 actual
'

test_expect_success 'rev-list --max-count=0 returns nothing' '
    (cd repo && grit rev-list --max-count=0 HEAD >../actual) &&
    test_line_count = 0 actual
'

test_expect_success 'rev-list --skip=8 on 8 commits returns nothing' '
    (cd repo && grit rev-list --skip=8 HEAD >../actual) &&
    test_line_count = 0 actual
'

test_expect_success 'rev-list --skip=7 returns 1 commit' '
    (cd repo && grit rev-list --skip=7 HEAD >../actual) &&
    test_line_count = 1 actual
'

test_expect_success 'rev-list --skip=2 --max-count=3' '
    (cd repo && grit rev-list --skip=2 --max-count=3 HEAD >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'rev-list --skip and --max-count exceed total' '
    (cd repo && grit rev-list --skip=6 --max-count=10 HEAD >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'rev-list with two ^exclude refs' '
    (cd repo &&
    B1=$(/usr/bin/git rev-parse branch1) &&
    B2=$(/usr/bin/git rev-parse branch2) &&
    grit rev-list HEAD ^"$B1" ^"$B2" >../actual) &&
    # base is ancestor of both branches, all branch commits excluded
    # remaining: M-first, merge-branch1, merge-branch2, M-final
    test_line_count = 4 actual
'

test_expect_success 'rev-list all commits have valid 40-char hex' '
    (cd repo && grit rev-list HEAD >../actual) &&
    while read hash; do
        echo "$hash" | grep -q "^[0-9a-f]\{40\}$" || exit 1
    done <actual
'

test_expect_success 'rev-list all hashes are unique' '
    (cd repo && grit rev-list HEAD >../actual) &&
    sort actual >sorted &&
    sort -u actual >unique &&
    test_cmp sorted unique
'

test_expect_success 'rev-list --count --first-parent consistent' '
    (cd repo && grit rev-list --first-parent HEAD >../fp_list &&
    grit rev-list --count --first-parent HEAD >../fp_count) &&
    LINES=$(wc -l <fp_list) &&
    COUNT=$(cat fp_count) &&
    test "$LINES" = "$COUNT"
'

test_expect_success 'rev-list HEAD contains all branch commits' '
    (cd repo && grit rev-list HEAD >../all) &&
    (cd repo && /usr/bin/git rev-parse branch1 >../b1) &&
    (cd repo && /usr/bin/git rev-parse branch2 >../b2) &&
    grep "$(cat b1)" all &&
    grep "$(cat b2)" all
'

test_expect_success 'rev-list date-order lists all commits' '
    (cd repo && grit rev-list --date-order HEAD >../actual) &&
    test_line_count = 8 actual
'

test_expect_success 'rev-list topo-order and default have same commits' '
    (cd repo && grit rev-list HEAD | sort >../default_sorted &&
    grit rev-list --topo-order HEAD | sort >../topo_sorted) &&
    test_cmp default_sorted topo_sorted
'

test_expect_success 'rev-list date-order and default have same commits' '
    (cd repo && grit rev-list HEAD | sort >../default_sorted &&
    grit rev-list --date-order HEAD | sort >../date_sorted) &&
    test_cmp default_sorted date_sorted
'

test_expect_success 'rev-list --max-count with --first-parent' '
    (cd repo && grit rev-list --max-count=2 --first-parent HEAD >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'rev-list --skip with --first-parent' '
    (cd repo && grit rev-list --skip=3 --first-parent HEAD >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'rev-list --count with ^exclude' '
    (cd repo && B1=$(/usr/bin/git rev-parse branch1) &&
    grit rev-list --count HEAD ^"$B1" >../actual) &&
    echo "5" >expect &&
    test_cmp expect actual
'

test_done
