#!/bin/sh

test_description='merge-file marker size, labels, conflict styles, and resolution modes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup conflict files' '
    echo "line1" >base.txt &&
    echo "line1-ours" >ours.txt &&
    echo "line1-theirs" >theirs.txt
'

test_expect_success 'merge-file -p with conflict exits 1' '
    cp ours.txt cur.txt &&
    test_must_fail grit merge-file -p cur.txt base.txt theirs.txt >actual 2>/dev/null &&
    grep "<<<<<<" actual
'

test_expect_success 'default conflict markers are 7 chars' '
    cp ours.txt cur.txt &&
    test_must_fail grit merge-file -p cur.txt base.txt theirs.txt >actual 2>/dev/null &&
    grep "^<<<<<<< " actual &&
    grep "^=======$" actual &&
    grep "^>>>>>>> " actual
'

test_expect_success 'marker-size 10 uses 10-char markers' '
    cp ours.txt cur.txt &&
    test_must_fail grit merge-file -p --marker-size 10 cur.txt base.txt theirs.txt >actual 2>/dev/null &&
    grep "^<<<<<<<<<< " actual &&
    grep "^==========$" actual &&
    grep "^>>>>>>>>>> " actual
'

test_expect_success 'marker-size 3 uses 3-char markers' '
    cp ours.txt cur.txt &&
    test_must_fail grit merge-file -p --marker-size 3 cur.txt base.txt theirs.txt >actual 2>/dev/null &&
    grep "^<<< " actual &&
    grep "^===$" actual &&
    grep "^>>> " actual
'

test_expect_success 'marker-size 1 uses 1-char markers' '
    cp ours.txt cur.txt &&
    test_must_fail grit merge-file -p --marker-size 1 cur.txt base.txt theirs.txt >actual 2>/dev/null &&
    grep "^< " actual &&
    grep "^=$" actual &&
    grep "^> " actual
'

test_expect_success '--theirs resolves to theirs side' '
    cp ours.txt cur.txt &&
    grit merge-file -p --theirs cur.txt base.txt theirs.txt >actual 2>/dev/null &&
    echo "line1-theirs" >expect &&
    test_cmp expect actual
'

test_expect_success '--theirs exits 0' '
    cp ours.txt cur.txt &&
    grit merge-file -p --theirs cur.txt base.txt theirs.txt >/dev/null 2>/dev/null
'

test_expect_success '--ours resolves to ours side' '
    cp ours.txt cur.txt &&
    grit merge-file -p --ours cur.txt base.txt theirs.txt >actual 2>/dev/null &&
    echo "line1-ours" >expect &&
    test_cmp expect actual
'

test_expect_success '--union includes both sides' '
    cp ours.txt cur.txt &&
    grit merge-file -p --union cur.txt base.txt theirs.txt >actual 2>/dev/null &&
    grep "line1-ours" actual &&
    grep "line1-theirs" actual
'

test_expect_success '--union exits 0' '
    cp ours.txt cur.txt &&
    grit merge-file -p --union cur.txt base.txt theirs.txt >/dev/null 2>/dev/null
'

test_expect_success '--diff3 shows base section' '
    cp ours.txt cur.txt &&
    test_must_fail grit merge-file -p --diff3 cur.txt base.txt theirs.txt >actual 2>/dev/null &&
    grep "^||||||| " actual &&
    grep "line1$" actual
'

test_expect_success '--diff3 still has ours and theirs' '
    cp ours.txt cur.txt &&
    test_must_fail grit merge-file -p --diff3 cur.txt base.txt theirs.txt >actual 2>/dev/null &&
    grep "line1-ours" actual &&
    grep "line1-theirs" actual
'

test_expect_success '-L sets custom labels' '
    cp ours.txt cur.txt &&
    test_must_fail grit merge-file -p -L "OURS" -L "BASE" -L "THEIRS" cur.txt base.txt theirs.txt >actual 2>/dev/null &&
    grep "<<<<<<< OURS" actual &&
    grep ">>>>>>> THEIRS" actual
'

test_expect_success '-L with --diff3 shows base label' '
    cp ours.txt cur.txt &&
    test_must_fail grit merge-file -p --diff3 -L "A" -L "O" -L "B" cur.txt base.txt theirs.txt >actual 2>/dev/null &&
    grep "<<<<<<< A" actual &&
    grep "||||||| O" actual &&
    grep ">>>>>>> B" actual
'

test_expect_success '-L with --marker-size combines both' '
    cp ours.txt cur.txt &&
    test_must_fail grit merge-file -p --marker-size 10 -L "MY" -L "ORIG" -L "YOUR" cur.txt base.txt theirs.txt >actual 2>/dev/null &&
    grep "^<<<<<<<<<< MY" actual &&
    grep "^>>>>>>>>>> YOUR" actual
'

test_expect_success '--quiet suppresses conflict warning' '
    cp ours.txt cur.txt &&
    test_must_fail grit merge-file -p --quiet cur.txt base.txt theirs.txt >actual 2>err &&
    ! grep "warning" err
'

test_expect_success 'no conflict when files are identical' '
    echo "same" >b.txt &&
    echo "same" >c1.txt &&
    echo "same" >c2.txt &&
    grit merge-file -p c1.txt b.txt c2.txt >actual 2>/dev/null &&
    echo "same" >expect &&
    test_cmp expect actual
'

test_expect_success 'no conflict exits 0' '
    echo "same" >b.txt &&
    echo "same" >c1.txt &&
    echo "same" >c2.txt &&
    grit merge-file -p c1.txt b.txt c2.txt >/dev/null 2>/dev/null
'

test_expect_success 'clean merge with non-overlapping changes' '
    printf "line1\nline2\nline3\n" >b2.txt &&
    printf "line1-modified\nline2\nline3\n" >c3.txt &&
    printf "line1\nline2\nline3-modified\n" >c4.txt &&
    grit merge-file -p c3.txt b2.txt c4.txt >actual 2>/dev/null &&
    printf "line1-modified\nline2\nline3-modified\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'clean merge exits 0' '
    printf "line1\nline2\nline3\n" >b2.txt &&
    printf "line1-modified\nline2\nline3\n" >c3.txt &&
    printf "line1\nline2\nline3-modified\n" >c4.txt &&
    grit merge-file -p c3.txt b2.txt c4.txt >/dev/null 2>/dev/null
'

test_expect_success 'without -p, merge overwrites current file' '
    echo "line1" >mb.txt &&
    echo "line1-ours" >mc.txt &&
    echo "line1-theirs" >mt.txt &&
    grit merge-file --theirs mc.txt mb.txt mt.txt 2>/dev/null &&
    cat mc.txt >actual &&
    echo "line1-theirs" >expect &&
    test_cmp expect actual
'

test_expect_success 'multi-line conflict with marker-size' '
    printf "a\nb\nc\n" >mb2.txt &&
    printf "a\nB\nc\n" >mc2.txt &&
    printf "a\nX\nc\n" >mt2.txt &&
    test_must_fail grit merge-file -p --marker-size 5 mc2.txt mb2.txt mt2.txt >actual 2>/dev/null &&
    grep "^<<<<< " actual &&
    grep "^=====$" actual &&
    grep "^>>>>> " actual
'

test_expect_success 'multi-line clean merge preserves context' '
    printf "a\nb\nc\nd\ne\n" >mb3.txt &&
    printf "a\nB\nc\nd\ne\n" >mc3.txt &&
    printf "a\nb\nc\nd\nE\n" >mt3.txt &&
    grit merge-file -p mc3.txt mb3.txt mt3.txt >actual 2>/dev/null &&
    printf "a\nB\nc\nd\nE\n" >expect &&
    test_cmp expect actual
'

test_expect_success '--zdiff3 shows zealous diff3 output' '
    cp ours.txt cur.txt &&
    test_must_fail grit merge-file -p --zdiff3 cur.txt base.txt theirs.txt >actual 2>/dev/null &&
    grep "<<<<<<" actual &&
    grep ">>>>>>>" actual
'

test_expect_success 'empty base with both sides adding content' '
    echo "" >eb.txt &&
    echo "ours-add" >ec.txt &&
    echo "theirs-add" >et.txt &&
    test_must_fail grit merge-file -p ec.txt eb.txt et.txt >actual 2>/dev/null &&
    grep "ours-add" actual &&
    grep "theirs-add" actual
'

test_expect_success 'both sides make same change is clean' '
    echo "original" >sb.txt &&
    echo "changed" >sc1.txt &&
    echo "changed" >sc2.txt &&
    grit merge-file -p sc1.txt sb.txt sc2.txt >actual 2>/dev/null &&
    echo "changed" >expect &&
    test_cmp expect actual
'

test_expect_success 'one side deletes while other keeps is conflict' '
    printf "line1\nline2\n" >db.txt &&
    printf "line1\n" >dc.txt &&
    printf "line1\nline2-modified\n" >dt.txt &&
    test_must_fail grit merge-file -p dc.txt db.txt dt.txt >actual 2>/dev/null &&
    test -s actual
'

test_expect_success '--ours with multi-line conflict keeps ours' '
    printf "a\nb\nc\n" >mob.txt &&
    printf "a\nB\nc\n" >moc.txt &&
    printf "a\nX\nc\n" >mot.txt &&
    grit merge-file -p --ours moc.txt mob.txt mot.txt >actual 2>/dev/null &&
    printf "a\nB\nc\n" >expect &&
    test_cmp expect actual
'

test_expect_success '--theirs with multi-line conflict keeps theirs' '
    printf "a\nb\nc\n" >mtb.txt &&
    printf "a\nB\nc\n" >mtc.txt &&
    printf "a\nX\nc\n" >mtt.txt &&
    grit merge-file -p --theirs mtc.txt mtb.txt mtt.txt >actual 2>/dev/null &&
    printf "a\nX\nc\n" >expect &&
    test_cmp expect actual
'

test_done
