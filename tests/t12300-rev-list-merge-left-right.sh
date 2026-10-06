#!/bin/sh
test_description='rev-list with merges: --first-parent, --count, ^exclude, merge parent detection'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with merge' '
    (
    grit init repo && cd repo &&
    git config user.email "t@t.com" &&
    git config user.name "T" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
    echo base >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700000000 +0000" GIT_COMMITTER_DATE="1700000000 +0000" \
    grit commit -m "base" &&
    /usr/bin/git checkout -b side &&
    echo side1 >side1.txt &&
    grit add side1.txt &&
    GIT_AUTHOR_DATE="1700001000 +0000" GIT_COMMITTER_DATE="1700001000 +0000" \
    grit commit -m "side-1" &&
    echo side2 >side2.txt &&
    grit add side2.txt &&
    GIT_AUTHOR_DATE="1700002000 +0000" GIT_COMMITTER_DATE="1700002000 +0000" \
    grit commit -m "side-2" &&
    /usr/bin/git checkout main &&
    echo main1 >main1.txt &&
    grit add main1.txt &&
    GIT_AUTHOR_DATE="1700003000 +0000" GIT_COMMITTER_DATE="1700003000 +0000" \
    grit commit -m "main-1" &&
    echo main2 >main2.txt &&
    grit add main2.txt &&
    GIT_AUTHOR_DATE="1700004000 +0000" GIT_COMMITTER_DATE="1700004000 +0000" \
    grit commit -m "main-2" &&
    /usr/bin/git merge side -m "merge-side"
    )
'

test_expect_success 'rev-list HEAD lists all commits including merge' '
    (cd repo && grit rev-list HEAD >../actual) &&
    test_line_count = 6 actual
'

test_expect_success 'rev-list --count HEAD counts all commits' '
    (cd repo && grit rev-list --count HEAD >../actual) &&
    echo "6" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list --first-parent excludes side branch' '
    (cd repo && grit rev-list --first-parent HEAD >../actual) &&
    test_line_count = 4 actual
'

test_expect_success 'rev-list --count --first-parent' '
    (cd repo && grit rev-list --count --first-parent HEAD >../actual) &&
    echo "4" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list --first-parent includes merge commit' '
    (cd repo && grit rev-list --first-parent HEAD >../actual) &&
    HEAD=$(cd repo && grit rev-parse HEAD) &&
    grep "$HEAD" actual
'

test_expect_success 'rev-list --first-parent does not include side tip' '
    (cd repo && grit rev-list --first-parent HEAD >../fp_list) &&
    SIDE=$(cd repo && /usr/bin/git rev-parse side) &&
    ! grep "$SIDE" fp_list
'

test_expect_success 'rev-list ^exclude removes reachable commits' '
    (cd repo && SIDE=$(/usr/bin/git rev-parse side) &&
    grit rev-list HEAD ^"$SIDE" >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'rev-list ^exclude: merge commit is in result' '
    (cd repo && SIDE=$(/usr/bin/git rev-parse side) &&
    grit rev-list HEAD ^"$SIDE" >../actual) &&
    HEAD=$(cd repo && grit rev-parse HEAD) &&
    grep "$HEAD" actual
'

test_expect_success 'rev-list ^exclude: base commit is excluded' '
    (cd repo && SIDE=$(/usr/bin/git rev-parse side) &&
    grit rev-list HEAD ^"$SIDE" >../actual) &&
    BASE=$(cd repo && grit rev-list --reverse HEAD | head -1) &&
    ! grep "$BASE" actual
'

test_expect_success 'merge commit has two parents in log format' '
    (cd repo && grit log -n 1 --format="%P" >../actual) &&
    set -- $(cat actual) &&
    test "$#" = 2
'

test_expect_success 'merge commit parents are valid hashes' '
    (cd repo && grit log -n 1 --format="%P" >../actual) &&
    for hash in $(cat actual); do
        echo "$hash" | grep -q "^[0-9a-f]\{40\}$" || exit 1
    done
'

test_expect_success 'non-merge commit has one parent' '
    (cd repo && grit log --skip=1 -n 1 --format="%P" >../actual) &&
    set -- $(cat actual) &&
    test "$#" = 1
'

test_expect_success 'root commit has no parents' '
    (cd repo && FIRST=$(grit rev-list --reverse HEAD | head -1) &&
    grit log -n 1 --format="%P" "$FIRST" >../actual) &&
    PARENTS=$(cat actual) &&
    test -z "$PARENTS"
'

test_expect_success 'rev-list --reverse starts with root' '
    (cd repo && grit rev-list --reverse HEAD >../actual) &&
    FIRST=$(head -1 actual) &&
    ROOT=$(cd repo && grit rev-list --reverse HEAD | head -1) &&
    test "$FIRST" = "$ROOT"
'

test_expect_success 'rev-list --topo-order lists all' '
    (cd repo && grit rev-list --topo-order HEAD >../actual) &&
    test_line_count = 6 actual
'

test_expect_success 'rev-list --skip=1 skips HEAD commit' '
    (cd repo && grit rev-list --skip=1 HEAD >../actual) &&
    HEAD=$(cd repo && grit rev-parse HEAD) &&
    ! grep "$HEAD" actual
'

test_expect_success 'rev-list --max-count=3 limits output' '
    (cd repo && grit rev-list --max-count=3 HEAD >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'rev-list --max-count=1 returns only HEAD' '
    (cd repo && grit rev-list --max-count=1 HEAD >../actual) &&
    test_line_count = 1 actual &&
    HEAD=$(cd repo && grit rev-parse HEAD) &&
    echo "$HEAD" >expect &&
    test_cmp expect actual
'

test_expect_success 'setup: second merge with divergence' '
    (cd repo &&
    /usr/bin/git checkout -b feature &&
    echo feat >feat.txt &&
    grit add feat.txt &&
    GIT_AUTHOR_DATE="1700006000 +0000" GIT_COMMITTER_DATE="1700006000 +0000" \
    grit commit -m "feature-1" &&
    /usr/bin/git checkout main &&
    echo main3 >main3.txt &&
    grit add main3.txt &&
    GIT_AUTHOR_DATE="1700007000 +0000" GIT_COMMITTER_DATE="1700007000 +0000" \
    grit commit -m "main-3" &&
    /usr/bin/git merge feature -m "merge-feature")
'

test_expect_success 'rev-list after second merge counts all' '
    (cd repo && grit rev-list --count HEAD >../actual) &&
    echo "9" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list --first-parent after two merges' '
    (cd repo && grit rev-list --first-parent HEAD >../actual) &&
    test_line_count = 6 actual
'

test_expect_success 'second merge commit also has two parents' '
    (cd repo && grit log -n 1 --format="%P" >../actual) &&
    set -- $(cat actual) &&
    test "$#" = 2
'

test_expect_success 'rev-list on side branch only' '
    (cd repo && grit rev-list side >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'rev-list side ^base shows side-only commits' '
    (cd repo &&
    BASE=$(grit rev-list --reverse HEAD | head -1) &&
    grit rev-list side ^"$BASE" >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'rev-list --count on side branch' '
    (cd repo && grit rev-list --count side >../actual) &&
    echo "3" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list all commits are unique hashes' '
    (cd repo && grit rev-list HEAD >../actual) &&
    sort actual >sorted &&
    sort -u actual >unique &&
    test_cmp sorted unique
'

test_expect_success 'rev-list all hashes are valid 40-char hex' '
    (cd repo && grit rev-list HEAD >../actual) &&
    while read hash; do
        echo "$hash" | grep -q "^[0-9a-f]\{40\}$" || exit 1
    done <actual
'

test_expect_success 'rev-list HEAD starts with HEAD hash' '
    (cd repo && grit rev-list HEAD >../actual &&
    grit rev-parse HEAD >../head_hash) &&
    FIRST=$(head -1 actual) &&
    HEAD=$(cat head_hash) &&
    test "$FIRST" = "$HEAD"
'

test_expect_success 'rev-list --skip and --max-count combined' '
    (cd repo && grit rev-list --skip=2 --max-count=3 HEAD >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'rev-list --reverse --first-parent starts with root' '
    (cd repo && grit rev-list --reverse --first-parent HEAD >../actual) &&
    ROOT=$(cd repo && grit rev-list --reverse HEAD | head -1) &&
    FIRST=$(head -1 actual) &&
    test "$FIRST" = "$ROOT"
'

test_expect_success 'rev-list --reverse --first-parent ends with HEAD' '
    (cd repo && grit rev-list --reverse --first-parent HEAD >../actual) &&
    HEAD=$(cd repo && grit rev-parse HEAD) &&
    LAST=$(tail -1 actual) &&
    test "$LAST" = "$HEAD"
'

test_expect_success 'rev-list feature branch has expected count' '
    (cd repo && grit rev-list --count feature >../actual) &&
    echo "7" >expect &&
    test_cmp expect actual
'

test_done
