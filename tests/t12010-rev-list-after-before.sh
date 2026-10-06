#!/bin/sh
test_description='rev-list ranges, exclusions, and ancestry-path'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup linear history with 6 commits' '
	(
    grit init repo &&
    cd repo &&
    git config user.email "t@t.com" &&
    git config user.name "T" &&
    echo a >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700000000 +0000" GIT_COMMITTER_DATE="1700000000 +0000" \
    grit commit -m "A" &&
    echo b >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700001000 +0000" GIT_COMMITTER_DATE="1700001000 +0000" \
    grit commit -m "B" &&
    echo c >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700002000 +0000" GIT_COMMITTER_DATE="1700002000 +0000" \
    grit commit -m "C" &&
    echo d >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700003000 +0000" GIT_COMMITTER_DATE="1700003000 +0000" \
    grit commit -m "D" &&
    echo e >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700004000 +0000" GIT_COMMITTER_DATE="1700004000 +0000" \
    grit commit -m "E" &&
    echo f >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700005000 +0000" GIT_COMMITTER_DATE="1700005000 +0000" \
    grit commit -m "F"
	)
'

# Save commit hashes for later
test_expect_success 'record commit hashes' '
    (cd repo && grit rev-list --reverse HEAD >../all_hashes) &&
    test_line_count = 6 all_hashes &&
    COMMIT_A=$(sed -n 1p all_hashes) &&
    COMMIT_B=$(sed -n 2p all_hashes) &&
    COMMIT_C=$(sed -n 3p all_hashes) &&
    COMMIT_D=$(sed -n 4p all_hashes) &&
    COMMIT_E=$(sed -n 5p all_hashes) &&
    COMMIT_F=$(sed -n 6p all_hashes) &&
    echo "$COMMIT_A" >hash_A &&
    echo "$COMMIT_B" >hash_B &&
    echo "$COMMIT_C" >hash_C &&
    echo "$COMMIT_D" >hash_D &&
    echo "$COMMIT_E" >hash_E &&
    echo "$COMMIT_F" >hash_F
'

test_expect_success 'range A..HEAD excludes A' '
    A=$(cat hash_A) &&
    (cd repo && grit rev-list $A..HEAD >../actual) &&
    test_line_count = 5 actual &&
    ! grep "$(cat hash_A)" actual
'

test_expect_success 'range B..HEAD excludes A and B' '
    B=$(cat hash_B) &&
    (cd repo && grit rev-list $B..HEAD >../actual) &&
    test_line_count = 4 actual &&
    ! grep "$(cat hash_A)" actual &&
    ! grep "$(cat hash_B)" actual
'

test_expect_success 'range C..E excludes A, B, C and F' '
    C=$(cat hash_C) &&
    E=$(cat hash_E) &&
    (cd repo && grit rev-list $C..$E >../actual) &&
    test_line_count = 2 actual &&
    ! grep "$(cat hash_A)" actual &&
    ! grep "$(cat hash_B)" actual &&
    ! grep "$(cat hash_C)" actual
'

test_expect_success 'range D..F shows exactly E and F' '
    D=$(cat hash_D) &&
    F=$(cat hash_F) &&
    (cd repo && grit rev-list $D..$F >../actual) &&
    test_line_count = 2 actual &&
    grep "$(cat hash_E)" actual &&
    grep "$(cat hash_F)" actual
'

test_expect_success 'HEAD ^A is same as A..HEAD' '
    A=$(cat hash_A) &&
    (cd repo && grit rev-list $A..HEAD >../range_out) &&
    (cd repo && grit rev-list HEAD ^$A >../caret_out) &&
    test_cmp range_out caret_out
'

test_expect_success 'HEAD ^B is same as B..HEAD' '
    B=$(cat hash_B) &&
    (cd repo && grit rev-list $B..HEAD >../range_out) &&
    (cd repo && grit rev-list HEAD ^$B >../caret_out) &&
    test_cmp range_out caret_out
'

test_expect_success 'range with --count' '
    C=$(cat hash_C) &&
    (cd repo && grit rev-list --count $C..HEAD >../actual) &&
    echo "3" >expect &&
    test_cmp expect actual
'

test_expect_success 'range with --reverse' '
    B=$(cat hash_B) &&
    (cd repo && grit rev-list --reverse $B..HEAD >../actual) &&
    test_line_count = 4 actual &&
    FIRST=$(head -1 actual) &&
    test "$FIRST" = "$(cat hash_C)"
'

test_expect_success 'range with --max-count' '
    A=$(cat hash_A) &&
    (cd repo && grit rev-list --max-count=2 $A..HEAD >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'range with --skip' '
    A=$(cat hash_A) &&
    (cd repo && grit rev-list --skip=2 $A..HEAD >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'range with --skip and --max-count' '
    A=$(cat hash_A) &&
    (cd repo && grit rev-list --skip=1 --max-count=2 $A..HEAD >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'empty range (same commit) produces no output' '
    F=$(cat hash_F) &&
    (cd repo && grit rev-list $F..$F >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'range HEAD..A is empty (A is ancestor of HEAD)' '
    A=$(cat hash_A) &&
    (cd repo && grit rev-list HEAD..$A >../actual) &&
    test_must_be_empty actual
'

test_expect_success '--ancestry-path with range' '
    A=$(cat hash_A) &&
    (cd repo && grit rev-list --ancestry-path $A..HEAD >../actual) &&
    test_line_count = 5 actual
'

test_expect_success '--ancestry-path C..HEAD on linear history' '
    C=$(cat hash_C) &&
    (cd repo && grit rev-list --ancestry-path $C..HEAD >../actual) &&
    test_line_count = 3 actual
'

test_expect_success '--ancestry-path with range matches range on linear history' '
    B=$(cat hash_B) &&
    (cd repo && grit rev-list $B..HEAD >../range_out) &&
    (cd repo && grit rev-list --ancestry-path $B..HEAD >../ancestry_out) &&
    test_cmp range_out ancestry_out
'

test_expect_success 'setup branch for range tests' '
    (cd repo &&
     B=$(cat ../hash_B) &&
     grit branch side $B)
'

test_expect_success 'rev-list side lists A and B' '
    (cd repo && grit rev-list side >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'range side..HEAD excludes side commits' '
    (cd repo && grit rev-list side..HEAD >../actual) &&
    test_line_count = 4 actual
'

test_expect_success 'range HEAD..side is empty (side is ancestor)' '
    (cd repo && grit rev-list HEAD..side >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'rev-list --count side..HEAD' '
    (cd repo && grit rev-list --count side..HEAD >../actual) &&
    echo "4" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list HEAD ^side same as side..HEAD' '
    (cd repo && grit rev-list side..HEAD >../range_out) &&
    SIDE=$(cd repo && grit rev-parse side) &&
    (cd repo && grit rev-list HEAD ^$SIDE >../caret_out) &&
    test_cmp range_out caret_out
'

test_expect_success 'rev-list --topo-order with range' '
    B=$(cat hash_B) &&
    (cd repo && grit rev-list --topo-order $B..HEAD >../actual) &&
    test_line_count = 4 actual
'

test_expect_success 'rev-list --reverse with range' '
    C=$(cat hash_C) &&
    (cd repo && grit rev-list --reverse $C..HEAD >../actual) &&
    FIRST=$(head -1 actual) &&
    test "$FIRST" = "$(cat hash_D)"
'

test_expect_success 'multiple exclusions: HEAD ^B ^D still works' '
    B=$(cat hash_B) &&
    D=$(cat hash_D) &&
    (cd repo && grit rev-list HEAD ^$D >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'rev-list with short hash as input' '
    F=$(cat hash_F) &&
    SHORT=$(echo "$F" | cut -c1-7) &&
    (cd repo && grit rev-list $SHORT >../actual) &&
    test_line_count = 6 actual
'

test_expect_success 'rev-list with HEAD~N notation' '
    (cd repo && grit rev-list HEAD~2..HEAD >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'rev-list with HEAD^ notation' '
    (cd repo && grit rev-list HEAD^..HEAD >../actual) &&
    test_line_count = 1 actual &&
    grep "$(cat hash_F)" actual
'

test_expect_success 'rev-list --count with HEAD~3..HEAD' '
    (cd repo && grit rev-list --count HEAD~3..HEAD >../actual) &&
    echo "3" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-list --first-parent with range' '
    A=$(cat hash_A) &&
    (cd repo && grit rev-list --first-parent $A..HEAD >../actual) &&
    test_line_count = 5 actual
'

test_expect_success 'rev-list --date-order with range' '
    B=$(cat hash_B) &&
    (cd repo && grit rev-list --date-order $B..HEAD >../actual) &&
    test_line_count = 4 actual
'

test_expect_success 'range A..B shows only B' '
    A=$(cat hash_A) &&
    B=$(cat hash_B) &&
    (cd repo && grit rev-list $A..$B >../actual) &&
    test_line_count = 1 actual &&
    grep "$(cat hash_B)" actual
'

test_expect_success 'range D..E shows only E' '
    D=$(cat hash_D) &&
    E=$(cat hash_E) &&
    (cd repo && grit rev-list $D..$E >../actual) &&
    test_line_count = 1 actual &&
    grep "$(cat hash_E)" actual
'

test_done
