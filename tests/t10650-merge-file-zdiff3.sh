#!/bin/sh
# Tests for grit merge-file with --zdiff3, --diff3, --ours, --theirs, --union.

test_description='grit merge-file zdiff3 and merge strategies'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup helpers
###########################################################################

# Create a standard 3-way merge scenario with a conflict
setup_conflict () {
	cat >base.txt <<-\EOF &&
	line 1
	line 2
	line 3
	line 4
	line 5
	EOF
	cat >ours.txt <<-\EOF &&
	line 1
	line 2 modified by us
	line 3
	line 4
	line 5
	EOF
	cat >theirs.txt <<-\EOF
	line 1
	line 2 modified by them
	line 3
	line 4
	line 5
	EOF
}

# Create a clean merge scenario (no conflict)
setup_clean () {
	cat >base.txt <<-\EOF &&
	line 1
	line 2
	line 3
	line 4
	line 5
	EOF
	cat >ours.txt <<-\EOF &&
	line 1
	line 2 changed by us
	line 3
	line 4
	line 5
	EOF
	cat >theirs.txt <<-\EOF
	line 1
	line 2
	line 3
	line 4 changed by them
	line 5
	EOF
}

###########################################################################
# Section 2: Basic merge-file (no conflicts)
###########################################################################

test_expect_success 'merge-file clean merge succeeds' '
	setup_clean &&
	cp ours.txt result.txt &&
	grit merge-file result.txt base.txt theirs.txt &&
	grep "line 2 changed by us" result.txt &&
	grep "line 4 changed by them" result.txt
'

test_expect_success 'merge-file clean merge matches git' '
	setup_clean &&
	cp ours.txt result.txt &&
	grit merge-file -p -L A -L O -L B result.txt base.txt theirs.txt >grit_out &&
	cp ours.txt result.txt &&
	"$REAL_GIT" merge-file -p -L A -L O -L B result.txt base.txt theirs.txt >git_out &&
	test_cmp git_out grit_out
'

test_expect_success 'merge-file -p sends to stdout' '
	setup_clean &&
	cp ours.txt result.txt &&
	grit merge-file -p result.txt base.txt theirs.txt >stdout_out &&
	grep "line 2 changed by us" stdout_out &&
	grep "line 4 changed by them" stdout_out
'

test_expect_success 'merge-file -p does not modify input file' '
	setup_clean &&
	cp ours.txt result.txt &&
	cp result.txt original.txt &&
	grit merge-file -p result.txt base.txt theirs.txt >/dev/null &&
	test_cmp original.txt result.txt
'

test_expect_success 'merge-file -p clean merge matches git' '
	setup_clean &&
	cp ours.txt result.txt &&
	grit merge-file -p -L X -L O -L Y result.txt base.txt theirs.txt >grit_out &&
	cp ours.txt result.txt &&
	"$REAL_GIT" merge-file -p -L X -L O -L Y result.txt base.txt theirs.txt >git_out &&
	test_cmp git_out grit_out
'

###########################################################################
# Section 3: Conflict detection
###########################################################################

test_expect_success 'merge-file conflict exits non-zero' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file result.txt base.txt theirs.txt
'

test_expect_success 'merge-file conflict produces conflict markers' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file result.txt base.txt theirs.txt &&
	grep "^<<<<<<<" result.txt &&
	grep "^=======" result.txt &&
	grep "^>>>>>>>" result.txt
'

test_expect_success 'merge-file conflict with labels matches git' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file -p -L A -L O -L B result.txt base.txt theirs.txt >grit_out &&
	cp ours.txt result.txt &&
	test_must_fail "$REAL_GIT" merge-file -p -L A -L O -L B result.txt base.txt theirs.txt >git_out &&
	test_cmp git_out grit_out
'

test_expect_success 'merge-file -p conflict with labels matches git' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file -p -L X -L O -L Y result.txt base.txt theirs.txt >grit_out &&
	cp ours.txt result.txt &&
	test_must_fail "$REAL_GIT" merge-file -p -L X -L O -L Y result.txt base.txt theirs.txt >git_out &&
	test_cmp git_out grit_out
'

###########################################################################
# Section 4: --diff3
###########################################################################

test_expect_success 'merge-file --diff3 shows base version' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file --diff3 result.txt base.txt theirs.txt &&
	grep "^|||||||" result.txt
'

test_expect_success 'merge-file --diff3 with labels matches git' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file --diff3 -p -L A -L O -L B result.txt base.txt theirs.txt >grit_out &&
	cp ours.txt result.txt &&
	test_must_fail "$REAL_GIT" merge-file --diff3 -p -L A -L O -L B result.txt base.txt theirs.txt >git_out &&
	test_cmp git_out grit_out
'

test_expect_success 'merge-file --diff3 -p with labels matches git' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file --diff3 -p -L X -L O -L Y result.txt base.txt theirs.txt >grit_out &&
	cp ours.txt result.txt &&
	test_must_fail "$REAL_GIT" merge-file --diff3 -p -L X -L O -L Y result.txt base.txt theirs.txt >git_out &&
	test_cmp git_out grit_out
'

###########################################################################
# Section 5: --zdiff3
###########################################################################

test_expect_success 'merge-file --zdiff3 shows base version marker' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file --zdiff3 result.txt base.txt theirs.txt &&
	grep "^|||||||" result.txt
'

test_expect_success 'merge-file --zdiff3 with labels matches git' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file --zdiff3 -p -L A -L O -L B result.txt base.txt theirs.txt >grit_out &&
	cp ours.txt result.txt &&
	test_must_fail "$REAL_GIT" merge-file --zdiff3 -p -L A -L O -L B result.txt base.txt theirs.txt >git_out &&
	test_cmp git_out grit_out
'

test_expect_success 'merge-file --zdiff3 -p with labels matches git' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file --zdiff3 -p -L X -L O -L Y result.txt base.txt theirs.txt >grit_out &&
	cp ours.txt result.txt &&
	test_must_fail "$REAL_GIT" merge-file --zdiff3 -p -L X -L O -L Y result.txt base.txt theirs.txt >git_out &&
	test_cmp git_out grit_out
'

test_expect_success 'merge-file --zdiff3 trims common lines from conflict' '
	cat >zbase.txt <<-\EOF &&
	aaa
	bbb
	ccc
	ddd
	eee
	EOF
	cat >zours.txt <<-\EOF &&
	aaa
	bbb
	XXX
	ddd
	eee
	EOF
	cat >ztheirs.txt <<-\EOF &&
	aaa
	bbb
	YYY
	ddd
	eee
	EOF
	cp zours.txt result.txt &&
	test_must_fail grit merge-file --zdiff3 -p -L A -L O -L B result.txt zbase.txt ztheirs.txt >grit_out &&
	cp zours.txt result.txt &&
	test_must_fail "$REAL_GIT" merge-file --zdiff3 -p -L A -L O -L B result.txt zbase.txt ztheirs.txt >git_out &&
	test_cmp git_out grit_out
'

###########################################################################
# Section 6: --ours
###########################################################################

test_expect_success 'merge-file --ours resolves conflict with our version' '
	setup_conflict &&
	cp ours.txt result.txt &&
	grit merge-file --ours result.txt base.txt theirs.txt &&
	grep "line 2 modified by us" result.txt &&
	! grep "<<<<<<<" result.txt
'

test_expect_success 'merge-file --ours matches git' '
	setup_conflict &&
	cp ours.txt result.txt &&
	grit merge-file --ours -p -L A -L O -L B result.txt base.txt theirs.txt >grit_out &&
	cp ours.txt result.txt &&
	"$REAL_GIT" merge-file --ours -p -L A -L O -L B result.txt base.txt theirs.txt >git_out &&
	test_cmp git_out grit_out
'

test_expect_success 'merge-file --ours exits zero (no conflict)' '
	setup_conflict &&
	cp ours.txt result.txt &&
	grit merge-file --ours result.txt base.txt theirs.txt
'

###########################################################################
# Section 7: --theirs
###########################################################################

test_expect_success 'merge-file --theirs resolves conflict with their version' '
	setup_conflict &&
	cp ours.txt result.txt &&
	grit merge-file --theirs result.txt base.txt theirs.txt &&
	grep "line 2 modified by them" result.txt &&
	! grep "<<<<<<<" result.txt
'

test_expect_success 'merge-file --theirs matches git' '
	setup_conflict &&
	cp ours.txt result.txt &&
	grit merge-file --theirs -p -L A -L O -L B result.txt base.txt theirs.txt >grit_out &&
	cp ours.txt result.txt &&
	"$REAL_GIT" merge-file --theirs -p -L A -L O -L B result.txt base.txt theirs.txt >git_out &&
	test_cmp git_out grit_out
'

###########################################################################
# Section 8: --union
###########################################################################

test_expect_success 'merge-file --union includes both versions' '
	setup_conflict &&
	cp ours.txt result.txt &&
	grit merge-file --union result.txt base.txt theirs.txt &&
	grep "line 2 modified by us" result.txt &&
	grep "line 2 modified by them" result.txt &&
	! grep "<<<<<<<" result.txt
'

test_expect_success 'merge-file --union matches git' '
	setup_conflict &&
	cp ours.txt result.txt &&
	grit merge-file --union -p -L A -L O -L B result.txt base.txt theirs.txt >grit_out &&
	cp ours.txt result.txt &&
	"$REAL_GIT" merge-file --union -p -L A -L O -L B result.txt base.txt theirs.txt >git_out &&
	test_cmp git_out grit_out
'

###########################################################################
# Section 9: -L labels
###########################################################################

test_expect_success 'merge-file -L sets custom labels in conflict markers' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file -L "OURS" -L "BASE" -L "THEIRS" result.txt base.txt theirs.txt &&
	grep "^<<<<<<< OURS" result.txt &&
	grep "^>>>>>>> THEIRS" result.txt
'

test_expect_success 'merge-file -L labels match git exactly' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file -p -L "MY" -L "ORIG" -L "YOUR" result.txt base.txt theirs.txt >grit_out &&
	cp ours.txt result.txt &&
	test_must_fail "$REAL_GIT" merge-file -p -L "MY" -L "ORIG" -L "YOUR" result.txt base.txt theirs.txt >git_out &&
	test_cmp git_out grit_out
'

###########################################################################
# Section 10: --marker-size
###########################################################################

test_expect_success 'merge-file --marker-size changes marker length' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file --marker-size 10 result.txt base.txt theirs.txt &&
	grep "^<<<<<<<<<<" result.txt &&
	grep "^==========" result.txt &&
	grep "^>>>>>>>>>>" result.txt
'

test_expect_success 'merge-file --marker-size with labels matches git' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file --marker-size 10 -p -L A -L O -L B result.txt base.txt theirs.txt >grit_out &&
	cp ours.txt result.txt &&
	test_must_fail "$REAL_GIT" merge-file --marker-size 10 -p -L A -L O -L B result.txt base.txt theirs.txt >git_out &&
	test_cmp git_out grit_out
'

###########################################################################
# Section 11: --quiet
###########################################################################

test_expect_success 'merge-file -q suppresses conflict warning' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file -q result.txt base.txt theirs.txt 2>err &&
	test_must_be_empty err
'

###########################################################################
# Section 12: Edge cases
###########################################################################

test_expect_success 'merge-file identical files produces clean merge' '
	echo "same content" >same1.txt &&
	echo "same content" >same2.txt &&
	echo "same content" >same3.txt &&
	grit merge-file same1.txt same2.txt same3.txt &&
	echo "same content" >expect &&
	test_cmp expect same1.txt
'

test_expect_success 'merge-file empty base with additions on both sides merges content' '
	>empty_base.txt &&
	echo "ours addition" >ours_add.txt &&
	echo "theirs addition" >theirs_add.txt &&
	cp ours_add.txt result.txt &&
	grit merge-file -p result.txt empty_base.txt theirs_add.txt >grit_out || true &&
	grep "ours addition" grit_out &&
	grep "theirs addition" grit_out
'

test_expect_success 'merge-file with all empty files' '
	>e1.txt &&
	>e2.txt &&
	>e3.txt &&
	grit merge-file e1.txt e2.txt e3.txt &&
	test_must_be_empty e1.txt
'

test_expect_success 'merge-file --ours with clean merge is identity' '
	setup_clean &&
	cp ours.txt result.txt &&
	grit merge-file --ours result.txt base.txt theirs.txt &&
	grep "line 2 changed by us" result.txt &&
	grep "line 4 changed by them" result.txt
'

test_done
