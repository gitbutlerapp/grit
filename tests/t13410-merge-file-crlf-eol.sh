#!/bin/sh
# Tests for grit merge-file with CRLF/LF line endings and various merge strategies.

test_description='grit merge-file CRLF EOL handling and merge strategies'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup: create base files for merge' '
	printf "line1\nline2\nline3\n" >base.txt &&
	printf "line1\nMODIFIED\nline3\n" >ours.txt &&
	printf "line1\nline2\nline3\nline4\n" >theirs.txt
'

###########################################################################
# Basic merge-file -p
###########################################################################

test_expect_success 'merge-file -p clean merge succeeds' '
	cp ours.txt ours-copy.txt &&
	cp base.txt base-copy.txt &&
	cp theirs.txt theirs-copy.txt &&
	grit merge-file -p ours-copy.txt base-copy.txt theirs-copy.txt >actual &&
	printf "line1\nMODIFIED\nline3\nline4\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'merge-file -p matches git output for clean merge' '
	cp ours.txt ours-copy.txt &&
	cp base.txt base-copy.txt &&
	cp theirs.txt theirs-copy.txt &&
	grit merge-file -p ours-copy.txt base-copy.txt theirs-copy.txt >actual &&
	cp ours.txt ours-copy2.txt &&
	cp base.txt base-copy2.txt &&
	cp theirs.txt theirs-copy2.txt &&
	"$REAL_GIT" merge-file -p ours-copy2.txt base-copy2.txt theirs-copy2.txt >expect &&
	test_cmp expect actual
'

test_expect_success 'merge-file -p returns 0 on clean merge' '
	cp ours.txt o.txt && cp base.txt b.txt && cp theirs.txt t.txt &&
	grit merge-file -p o.txt b.txt t.txt >actual
'

###########################################################################
# Conflict detection
###########################################################################

test_expect_success 'setup: conflicting files' '
	printf "line1\nline2\nline3\n" >cbase.txt &&
	printf "line1\nOURS\nline3\n" >cours.txt &&
	printf "line1\nTHEIRS\nline3\n" >ctheirs.txt
'

test_expect_success 'merge-file -p with conflict returns non-zero' '
	cp cours.txt co.txt && cp cbase.txt cb.txt && cp ctheirs.txt ct.txt &&
	test_must_fail grit merge-file -p co.txt cb.txt ct.txt >actual
'

test_expect_success 'conflict output has markers' '
	cp cours.txt co.txt && cp cbase.txt cb.txt && cp ctheirs.txt ct.txt &&
	grit merge-file -p co.txt cb.txt ct.txt >actual || true &&
	grep "<<<<<<<" actual &&
	grep "=======" actual &&
	grep ">>>>>>>" actual
'

test_expect_success 'conflict markers contain filenames' '
	cp cours.txt co.txt && cp cbase.txt cb.txt && cp ctheirs.txt ct.txt &&
	grit merge-file -p co.txt cb.txt ct.txt >actual || true &&
	grep "<<<<<<< co.txt" actual &&
	grep ">>>>>>> ct.txt" actual
'

###########################################################################
# --ours, --theirs, --union strategies
###########################################################################

test_expect_success 'merge-file -p --ours resolves to our version' '
	cp cours.txt co.txt && cp cbase.txt cb.txt && cp ctheirs.txt ct.txt &&
	grit merge-file -p --ours co.txt cb.txt ct.txt >actual &&
	printf "line1\nOURS\nline3\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'merge-file -p --theirs resolves to their version' '
	cp cours.txt co.txt && cp cbase.txt cb.txt && cp ctheirs.txt ct.txt &&
	grit merge-file -p --theirs co.txt cb.txt ct.txt >actual &&
	printf "line1\nTHEIRS\nline3\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'merge-file -p --union includes both sides' '
	cp cours.txt co.txt && cp cbase.txt cb.txt && cp ctheirs.txt ct.txt &&
	grit merge-file -p --union co.txt cb.txt ct.txt >actual &&
	grep "OURS" actual &&
	grep "THEIRS" actual &&
	! grep "<<<<<<<" actual
'

###########################################################################
# --diff3 and --zdiff3
###########################################################################

test_expect_success 'merge-file -p --diff3 shows base in conflict' '
	cp cours.txt co.txt && cp cbase.txt cb.txt && cp ctheirs.txt ct.txt &&
	grit merge-file -p --diff3 co.txt cb.txt ct.txt >actual || true &&
	grep "|||||||" actual
'

test_expect_success 'merge-file -p --diff3 matches git' '
	cp cours.txt ours-d3.txt && cp cbase.txt base-d3.txt && cp ctheirs.txt theirs-d3.txt &&
	grit merge-file -p --diff3 ours-d3.txt base-d3.txt theirs-d3.txt >actual 2>/dev/null || true &&
	cp cours.txt ours-d3.txt && cp cbase.txt base-d3.txt && cp ctheirs.txt theirs-d3.txt &&
	"$REAL_GIT" merge-file -p --diff3 ours-d3.txt base-d3.txt theirs-d3.txt >expect 2>/dev/null || true &&
	test_cmp expect actual
'

test_expect_success 'merge-file -p --zdiff3 works' '
	cp cours.txt co.txt && cp cbase.txt cb.txt && cp ctheirs.txt ct.txt &&
	grit merge-file -p --zdiff3 co.txt cb.txt ct.txt >actual 2>/dev/null || true &&
	grep "|||||||" actual
'

###########################################################################
# Labels with -L
###########################################################################

test_expect_success 'merge-file -p -L uses custom labels' '
	cp cours.txt co.txt && cp cbase.txt cb.txt && cp ctheirs.txt ct.txt &&
	grit merge-file -p -L MINE -L BASE -L YOURS co.txt cb.txt ct.txt >actual || true &&
	grep "<<<<<<< MINE" actual &&
	grep ">>>>>>> YOURS" actual
'

test_expect_success 'merge-file -L matches git labels' '
	cp cours.txt co.txt && cp cbase.txt cb.txt && cp ctheirs.txt ct.txt &&
	grit merge-file -p -L A -L B -L C co.txt cb.txt ct.txt >actual 2>/dev/null || true &&
	cp cours.txt co2.txt && cp cbase.txt cb2.txt && cp ctheirs.txt ct2.txt &&
	"$REAL_GIT" merge-file -p -L A -L B -L C co2.txt cb2.txt ct2.txt >expect 2>/dev/null || true &&
	test_cmp expect actual
'

###########################################################################
# --marker-size
###########################################################################

test_expect_success 'merge-file --marker-size changes marker length' '
	cp cours.txt co.txt && cp cbase.txt cb.txt && cp ctheirs.txt ct.txt &&
	grit merge-file -p --marker-size 3 co.txt cb.txt ct.txt >actual 2>/dev/null || true &&
	grep "<<<" actual &&
	! grep "<<<<" actual
'

###########################################################################
# -q / --quiet
###########################################################################

test_expect_success 'merge-file -q suppresses warnings' '
	cp cours.txt co.txt && cp cbase.txt cb.txt && cp ctheirs.txt ct.txt &&
	grit merge-file -p -q co.txt cb.txt ct.txt >actual 2>err || true &&
	test_must_be_empty err
'

###########################################################################
# CRLF line ending handling
###########################################################################

test_expect_success 'setup: create CRLF files' '
	printf "line1\r\nline2\r\nline3\r\n" >crlf-base.txt &&
	printf "line1\r\nMODIFIED\r\nline3\r\n" >crlf-ours.txt &&
	printf "line1\r\nline2\r\nline3\r\nline4\r\n" >crlf-theirs.txt
'

test_expect_success 'merge-file -p with CRLF clean merge succeeds' '
	cp crlf-ours.txt co.txt && cp crlf-base.txt cb.txt && cp crlf-theirs.txt ct.txt &&
	grit merge-file -p co.txt cb.txt ct.txt >actual &&
	grep "MODIFIED" actual &&
	grep "line4" actual
'

test_expect_success 'merge-file CRLF output matches git' '
	cp crlf-ours.txt co.txt && cp crlf-base.txt cb.txt && cp crlf-theirs.txt ct.txt &&
	grit merge-file -p co.txt cb.txt ct.txt >actual &&
	cp crlf-ours.txt co2.txt && cp crlf-base.txt cb2.txt && cp crlf-theirs.txt ct2.txt &&
	"$REAL_GIT" merge-file -p co2.txt cb2.txt ct2.txt >expect &&
	test_cmp expect actual
'

test_expect_success 'setup: CRLF conflict files' '
	printf "line1\r\nline2\r\nline3\r\n" >crlf-cbase.txt &&
	printf "line1\r\nOURS\r\nline3\r\n" >crlf-cours.txt &&
	printf "line1\r\nTHEIRS\r\nline3\r\n" >crlf-ctheirs.txt
'

test_expect_success 'merge-file CRLF conflict has markers' '
	cp crlf-cours.txt co.txt && cp crlf-cbase.txt cb.txt && cp crlf-ctheirs.txt ct.txt &&
	grit merge-file -p co.txt cb.txt ct.txt >actual || true &&
	grep "<<<<<<<" actual
'

test_expect_success 'merge-file CRLF conflict contains both sides' '
	cp crlf-cours.txt crlf-co.txt && cp crlf-cbase.txt crlf-cb.txt && cp crlf-ctheirs.txt crlf-ct.txt &&
	grit merge-file -p crlf-co.txt crlf-cb.txt crlf-ct.txt >actual 2>/dev/null || true &&
	grep "OURS" actual &&
	grep "THEIRS" actual
'

test_expect_success 'merge-file CRLF --ours matches git' '
	cp crlf-cours.txt co.txt && cp crlf-cbase.txt cb.txt && cp crlf-ctheirs.txt ct.txt &&
	grit merge-file -p --ours co.txt cb.txt ct.txt >actual &&
	cp crlf-cours.txt co2.txt && cp crlf-cbase.txt cb2.txt && cp crlf-ctheirs.txt ct2.txt &&
	"$REAL_GIT" merge-file -p --ours co2.txt cb2.txt ct2.txt >expect &&
	test_cmp expect actual
'

###########################################################################
# Mixed LF/CRLF
###########################################################################

test_expect_success 'merge-file with mixed line endings' '
	printf "line1\nline2\r\nline3\n" >mix-base.txt &&
	printf "line1\nMIXED\r\nline3\n" >mix-ours.txt &&
	printf "line1\nline2\r\nline3\nline4\n" >mix-theirs.txt &&
	cp mix-ours.txt co.txt && cp mix-base.txt cb.txt && cp mix-theirs.txt ct.txt &&
	grit merge-file -p co.txt cb.txt ct.txt >actual &&
	grep "MIXED" actual &&
	grep "line4" actual
'

###########################################################################
# In-place (without -p)
###########################################################################

test_expect_success 'merge-file without -p overwrites current file' '
	printf "line1\nline2\nline3\n" >ip-base.txt &&
	printf "line1\nMODIFIED\nline3\n" >ip-ours.txt &&
	printf "line1\nline2\nline3\nline4\n" >ip-theirs.txt &&
	grit merge-file ip-ours.txt ip-base.txt ip-theirs.txt &&
	grep "MODIFIED" ip-ours.txt &&
	grep "line4" ip-ours.txt
'

###########################################################################
# Edge cases
###########################################################################

test_expect_success 'merge-file identical files produces no change' '
	printf "same\n" >id-base.txt &&
	printf "same\n" >id-ours.txt &&
	printf "same\n" >id-theirs.txt &&
	grit merge-file -p id-ours.txt id-base.txt id-theirs.txt >actual &&
	printf "same\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'merge-file with empty base' '
	printf "" >eb-base.txt &&
	printf "ours\n" >eb-ours.txt &&
	printf "theirs\n" >eb-theirs.txt &&
	grit merge-file -p eb-ours.txt eb-base.txt eb-theirs.txt >actual 2>/dev/null || true &&
	test -s actual
'

test_expect_success 'merge-file missing file fails' '
	test_must_fail grit merge-file -p nonexistent.txt base.txt theirs.txt
'

test_done
