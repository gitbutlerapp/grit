#!/bin/sh

test_description='merge-file: three-way merges, conflicts, --ours/--theirs/--union, labels, diff3, marker-size'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup base files for clean merge' '
    printf "line1\nline2\nline3\n" >base &&
    printf "line1\nmodified\nline3\n" >ours &&
    printf "line1\nline2\nline3\nline4\n" >theirs
'

test_expect_success 'clean merge with -p to stdout' '
    cp ours ours_copy &&
    grit merge-file -p ours_copy base theirs >actual &&
    printf "line1\nmodified\nline3\nline4\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'clean merge modifies current-file in place' '
    cp ours ours_inplace &&
    grit merge-file ours_inplace base theirs &&
    printf "line1\nmodified\nline3\nline4\n" >expect &&
    test_cmp expect ours_inplace
'

test_expect_success 'setup conflicting files' '
    printf "line1\nline2\nline3\n" >cbase &&
    printf "line1\nours-change\nline3\n" >cours &&
    printf "line1\ntheirs-change\nline3\n" >ctheirs
'

test_expect_success 'conflicting merge returns non-zero' '
    cp cours cours_copy &&
    test_must_fail grit merge-file -p cours_copy cbase ctheirs >actual 2>err
'

test_expect_success 'conflict output contains markers' '
    grep "<<<<<<" actual &&
    grep "======" actual &&
    grep ">>>>>>" actual
'

test_expect_success 'conflict contains ours content' '
    grep "ours-change" actual
'

test_expect_success 'conflict contains theirs content' '
    grep "theirs-change" actual
'

test_expect_success 'non-conflicting lines preserved in conflict output' '
    grep "line1" actual &&
    grep "line3" actual
'

test_expect_success '--ours resolves conflict with our version' '
    cp cours cours_ours &&
    grit merge-file -p --ours cours_ours cbase ctheirs >actual &&
    printf "line1\nours-change\nline3\n" >expect &&
    test_cmp expect actual
'

test_expect_success '--theirs resolves conflict with their version' '
    cp cours cours_theirs &&
    grit merge-file -p --theirs cours_theirs cbase ctheirs >actual &&
    printf "line1\ntheirs-change\nline3\n" >expect &&
    test_cmp expect actual
'

test_expect_success '--union includes both sides' '
    cp cours cours_union &&
    grit merge-file -p --union cours_union cbase ctheirs >actual &&
    grep "ours-change" actual &&
    grep "theirs-change" actual &&
    ! grep "<<<<<<" actual
'

test_expect_success '--diff3 shows base in conflict' '
    cp cours cours_diff3 &&
    test_must_fail grit merge-file -p --diff3 cours_diff3 cbase ctheirs >actual 2>err &&
    grep "||||||" actual &&
    grep "line2" actual
'

test_expect_success '--zdiff3 shows zealous diff3 output' '
    cp cours cours_zdiff3 &&
    test_must_fail grit merge-file -p --zdiff3 cours_zdiff3 cbase ctheirs >actual 2>err &&
    grep "||||||" actual
'

test_expect_success '-L sets custom labels for conflict markers' '
    cp cours cours_label &&
    test_must_fail grit merge-file -p -L "OUR_FILE" -L "BASE_FILE" -L "THEIR_FILE" cours_label cbase ctheirs >actual 2>err &&
    grep "OUR_FILE" actual &&
    grep "THEIR_FILE" actual
'

test_expect_success '--marker-size changes marker width' '
    cp cours cours_marker &&
    test_must_fail grit merge-file -p --marker-size 3 cours_marker cbase ctheirs >actual 2>err &&
    grep "<<<" actual &&
    ! grep "<<<<<<<" actual
'

test_expect_success '--marker-size 10 makes longer markers' '
    cp cours cours_m10 &&
    test_must_fail grit merge-file -p --marker-size 10 cours_m10 cbase ctheirs >actual 2>err &&
    grep "<<<<<<<<<<" actual
'

test_expect_success 'quiet mode suppresses warnings' '
    cp cours cours_quiet &&
    test_must_fail grit merge-file -p -q cours_quiet cbase ctheirs >actual 2>err &&
    ! test -s err
'

test_expect_success 'non-quiet mode shows warnings' '
    cp cours cours_nonq &&
    test_must_fail grit merge-file -p cours_nonq cbase ctheirs >actual 2>err &&
    test -s err
'

test_expect_success 'setup: identical changes on both sides (no conflict)' '
    printf "line1\nline2\nline3\n" >ibase &&
    printf "line1\nsame-change\nline3\n" >iours &&
    printf "line1\nsame-change\nline3\n" >itheirs
'

test_expect_success 'identical changes merge cleanly' '
    cp iours iours_copy &&
    grit merge-file -p iours_copy ibase itheirs >actual &&
    printf "line1\nsame-change\nline3\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'setup: one side unchanged' '
    printf "aaa\nbbb\nccc\n" >ubase &&
    printf "aaa\nbbb\nccc\n" >uours &&
    printf "aaa\nBBB\nccc\n" >utheirs
'

test_expect_success 'only-theirs change merges cleanly' '
    cp uours uours_copy &&
    grit merge-file -p uours_copy ubase utheirs >actual &&
    printf "aaa\nBBB\nccc\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'only-ours change merges cleanly' '
    printf "aaa\nbbb\nccc\n" >u2base &&
    printf "aaa\nOUR\nccc\n" >u2ours &&
    printf "aaa\nbbb\nccc\n" >u2theirs &&
    cp u2ours u2ours_copy &&
    grit merge-file -p u2ours_copy u2base u2theirs >actual &&
    printf "aaa\nOUR\nccc\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'setup: multi-line additions' '
    printf "start\nend\n" >mbase &&
    printf "start\nours-1\nours-2\nend\n" >mours &&
    printf "start\nend\ntheirs-1\n" >mtheirs
'

test_expect_success 'multi-line additions from both sides merge' '
    cp mours mours_copy &&
    grit merge-file -p mours_copy mbase mtheirs >actual &&
    grep "theirs-1" actual &&
    grep "start" actual &&
    grep "end" actual
'

test_expect_success 'setup: deletions' '
    printf "keep\ndelete-me\nkeep2\n" >dbase &&
    printf "keep\nkeep2\n" >dours &&
    printf "keep\ndelete-me\nkeep2\n" >dtheirs
'

test_expect_success 'deletion by ours merges cleanly' '
    cp dours dours_copy &&
    grit merge-file -p dours_copy dbase dtheirs >actual &&
    printf "keep\nkeep2\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'setup: large file merge' '
    seq 1 100 >lbase &&
    seq 1 100 | sed "s/50/fifty/" >lours &&
    seq 1 100 | sed "s/75/seventy-five/" >ltheirs
'

test_expect_success 'large file merges cleanly' '
    cp lours lours_copy &&
    grit merge-file -p lours_copy lbase ltheirs >actual &&
    grep "fifty" actual &&
    grep "seventy-five" actual
'

test_expect_success 'setup: empty base file' '
    printf "" >ebase &&
    printf "ours-content\n" >eours &&
    printf "theirs-content\n" >etheirs
'

test_expect_success 'both sides adding to empty base produces conflict' '
    cp eours eours_copy &&
    test_must_fail grit merge-file -p eours_copy ebase etheirs >actual 2>err &&
    grep "ours-content" actual &&
    grep "theirs-content" actual
'

test_expect_success 'setup: single line files' '
    printf "original\n" >sbase &&
    printf "ours-ver\n" >sours &&
    printf "theirs-ver\n" >stheirs
'

test_expect_success 'single line conflict has markers' '
    cp sours sours_copy &&
    test_must_fail grit merge-file -p sours_copy sbase stheirs >actual 2>err &&
    grep "<<<<<<" actual &&
    grep "ours-ver" actual &&
    grep "theirs-ver" actual
'

test_expect_success '--ours on single line file' '
    cp sours sours_o &&
    grit merge-file -p --ours sours_o sbase stheirs >actual &&
    printf "ours-ver\n" >expect &&
    test_cmp expect actual
'

test_expect_success '--theirs on single line file' '
    cp sours sours_t &&
    grit merge-file -p --theirs sours_t sbase stheirs >actual &&
    printf "theirs-ver\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'merge-file with missing file fails' '
    test_must_fail grit merge-file -p nonexistent sbase stheirs 2>err &&
    test -s err
'

test_expect_success '-L with diff3 shows all three labels' '
    cp cours cours_3l &&
    test_must_fail grit merge-file -p --diff3 -L "A" -L "B" -L "C" cours_3l cbase ctheirs >actual 2>err &&
    grep "A" actual &&
    grep "B" actual &&
    grep "C" actual
'

test_done
