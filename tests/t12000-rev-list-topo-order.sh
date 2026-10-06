#!/bin/sh
test_description='rev-list --topo-order, --reverse, --count, --max-count, --skip'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup linear history' '
	(
    grit init repo &&
    cd repo &&
    git config user.email "t@t.com" &&
    git config user.name "T" &&
    echo a >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700000000 +0000" GIT_COMMITTER_DATE="1700000000 +0000" \
    grit commit -m "first" &&
    echo b >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700001000 +0000" GIT_COMMITTER_DATE="1700001000 +0000" \
    grit commit -m "second" &&
    echo c >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700002000 +0000" GIT_COMMITTER_DATE="1700002000 +0000" \
    grit commit -m "third" &&
    echo d >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700003000 +0000" GIT_COMMITTER_DATE="1700003000 +0000" \
    grit commit -m "fourth" &&
    echo e >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700004000 +0000" GIT_COMMITTER_DATE="1700004000 +0000" \
    grit commit -m "fifth"
	)
'

test_expect_success 'rev-list HEAD lists all commits' '
    (cd repo && grit rev-list HEAD >../actual) &&
    test_line_count = 5 actual
'

test_expect_success 'rev-list HEAD outputs full hashes' '
    (cd repo && grit rev-list HEAD >../actual) &&
    while read hash; do
        echo "$hash" | grep "^[0-9a-f]\{40\}$" || exit 1
    done <actual
'

test_expect_success 'rev-list HEAD is newest-first by default' '
    (cd repo && grit rev-list HEAD >../actual) &&
    FIRST=$(head -1 actual) &&
    HEAD=$(cd repo && grit rev-parse HEAD) &&
    test "$FIRST" = "$HEAD"
'

test_expect_success 'rev-list --topo-order lists all commits' '
    (cd repo && grit rev-list --topo-order HEAD >../actual) &&
    test_line_count = 5 actual
'

test_expect_success 'rev-list --topo-order on linear history matches default' '
    (cd repo && grit rev-list HEAD >../default_order) &&
    (cd repo && grit rev-list --topo-order HEAD >../topo_order) &&
    test_cmp default_order topo_order
'

test_expect_success 'rev-list --reverse shows oldest first' '
    (cd repo && grit rev-list --reverse HEAD >../actual) &&
    test_line_count = 5 actual &&
    FIRST=$(head -1 actual) &&
    LAST=$(tail -1 actual) &&
    HEAD=$(cd repo && grit rev-parse HEAD) &&
    test "$LAST" = "$HEAD"
'

test_expect_success 'rev-list --reverse is exact reverse of default' '
    (cd repo && grit rev-list HEAD >../default_order) &&
    (cd repo && grit rev-list --reverse HEAD >../reverse_order) &&
    tac default_order >default_reversed &&
    test_cmp default_reversed reverse_order
'

test_expect_success 'rev-list --count HEAD' '
    (cd repo && grit rev-list --count HEAD >../actual) &&
    echo "5" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list --max-count=3 limits output' '
    (cd repo && grit rev-list --max-count=3 HEAD >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'rev-list --max-count=1 shows only HEAD' '
    (cd repo && grit rev-list --max-count=1 HEAD >../actual) &&
    test_line_count = 1 actual &&
    HEAD=$(cd repo && grit rev-parse HEAD) &&
    echo "$HEAD" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list --max-count=0 shows nothing' '
    (cd repo && grit rev-list --max-count=0 HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'rev-list --max-count larger than history shows all' '
    (cd repo && grit rev-list --max-count=100 HEAD >../actual) &&
    test_line_count = 5 actual
'

test_expect_success 'rev-list --skip=1 skips first commit' '
    (cd repo && grit rev-list --skip=1 HEAD >../actual) &&
    test_line_count = 4 actual
'

test_expect_success 'rev-list --skip=3 skips three' '
    (cd repo && grit rev-list --skip=3 HEAD >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'rev-list --skip=5 shows nothing' '
    (cd repo && grit rev-list --skip=5 HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'rev-list --skip=100 shows nothing' '
    (cd repo && grit rev-list --skip=100 HEAD >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'rev-list --skip=1 --max-count=2' '
    (cd repo && grit rev-list --skip=1 --max-count=2 HEAD >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'rev-list --skip and --max-count together slice correctly' '
    (cd repo && grit rev-list HEAD >../all) &&
    (cd repo && grit rev-list --skip=1 --max-count=2 HEAD >../actual) &&
    sed -n "2,3p" all >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list commit..HEAD range' '
    FIRST=$(cd repo && grit rev-list --reverse HEAD | head -1) &&
    (cd repo && grit rev-list $FIRST..HEAD >../actual) &&
    test_line_count = 4 actual
'

test_expect_success 'rev-list commit..HEAD excludes start' '
    FIRST=$(cd repo && grit rev-list --reverse HEAD | head -1) &&
    (cd repo && grit rev-list $FIRST..HEAD >../actual) &&
    ! grep "$FIRST" actual
'

test_expect_success 'rev-list HEAD ^commit is same as commit..HEAD' '
    FIRST=$(cd repo && grit rev-list --reverse HEAD | head -1) &&
    (cd repo && grit rev-list $FIRST..HEAD >../range_actual) &&
    (cd repo && grit rev-list HEAD ^$FIRST >../caret_actual) &&
    test_cmp range_actual caret_actual
'

test_expect_success 'rev-list --count with range' '
    FIRST=$(cd repo && grit rev-list --reverse HEAD | head -1) &&
    (cd repo && grit rev-list --count $FIRST..HEAD >../actual) &&
    echo "4" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list --all lists all commits' '
    (cd repo && grit rev-list --all >../actual) &&
    test_line_count = 5 actual
'

test_expect_success 'rev-list --all matches HEAD on single branch' '
    (cd repo && grit rev-list HEAD >../head_list) &&
    (cd repo && grit rev-list --all >../all_list) &&
    test_cmp head_list all_list
'

test_expect_success 'rev-list --first-parent on linear history matches default' '
    (cd repo && grit rev-list HEAD >../default_order) &&
    (cd repo && grit rev-list --first-parent HEAD >../fp_order) &&
    test_cmp default_order fp_order
'

test_expect_success 'rev-list --reverse --topo-order' '
    (cd repo && grit rev-list --reverse --topo-order HEAD >../actual) &&
    test_line_count = 5 actual &&
    FIRST=$(head -1 actual) &&
    OLDEST=$(cd repo && grit rev-list --reverse HEAD | head -1) &&
    test "$FIRST" = "$OLDEST"
'

test_expect_success 'rev-list --date-order on linear history' '
    (cd repo && grit rev-list --date-order HEAD >../actual) &&
    test_line_count = 5 actual
'

test_expect_success 'rev-list --topo-order --count' '
    (cd repo && grit rev-list --count --topo-order HEAD >../actual) &&
    echo "5" >expect &&
    test_cmp expect actual
'

test_expect_success 'setup branch for topology tests' '
    (cd repo &&
     SECOND=$(grit rev-list --reverse HEAD | sed -n 2p) &&
     grit branch side $SECOND)
'

test_expect_success 'rev-list branch ref resolves correctly' '
    (cd repo && grit rev-list side >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'rev-list --count on branch' '
    (cd repo && grit rev-list --count side >../actual) &&
    echo "2" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list side..HEAD shows commits not on side' '
    (cd repo && grit rev-list side..HEAD >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'rev-list --reverse --max-count=2 HEAD' '
    (cd repo && grit rev-list --reverse --max-count=2 HEAD >../actual) &&
    test_line_count = 2 actual
'

test_done
