#!/bin/sh
# Tests for merge-file --ours, --theirs, --union, --stdout, --diff3,
# --zdiff3, --quiet, -L labels, --marker-size, clean merges, and
# conflict detection.

test_description='merge-file --ours, --theirs, --union and friends'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── setup helpers ─────────────────────────────────────────────────────────────

# Create a simple conflict scenario:
#   base: line1, line2, line3
#   ours: line1, line2-OURS, line3
#   theirs: line1, line2-THEIRS, line3
make_conflict () {
	cat >base.txt <<-\EOF &&
	line 1
	line 2
	line 3
	EOF
	cat >ours.txt <<-\EOF &&
	line 1
	line 2 OURS
	line 3
	EOF
	cat >theirs.txt <<-\EOF
	line 1
	line 2 THEIRS
	line 3
	EOF
}

# Create a clean merge scenario:
#   base: A, B, C, D
#   ours: A, B-modified, C, D
#   theirs: A, B, C, D-modified
make_clean () {
	cat >base.txt <<-\EOF &&
	alpha
	bravo
	charlie
	delta
	EOF
	cat >ours.txt <<-\EOF &&
	alpha
	bravo MODIFIED
	charlie
	delta
	EOF
	cat >theirs.txt <<-\EOF
	alpha
	bravo
	charlie
	delta MODIFIED
	EOF
}

test_expect_success 'setup' '
	mkdir -p work
'

# ── clean merge (no conflict) ────────────────────────────────────────────────

test_expect_success 'clean merge exits 0' '
	(
	cd work &&
	make_clean &&
	cp ours.txt result.txt &&
	grit merge-file result.txt base.txt theirs.txt
	)
'

test_expect_success 'clean merge preserves ours changes' '
	(
	cd work &&
	grep "bravo MODIFIED" result.txt
	)
'

test_expect_success 'clean merge includes theirs changes' '
	(
	cd work &&
	grep "delta MODIFIED" result.txt
	)
'

test_expect_success 'clean merge preserves unchanged lines' '
	(
	cd work &&
	grep "alpha" result.txt &&
	grep "charlie" result.txt
	)
'

# ── conflict detection ───────────────────────────────────────────────────────

test_expect_success 'conflicting merge exits non-zero' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file result.txt base.txt theirs.txt
	)
'

test_expect_success 'conflict markers present in output' '
	(
	cd work &&
	grep "^<<<<<<<" result.txt &&
	grep "^=======" result.txt &&
	grep "^>>>>>>>" result.txt
	)
'

test_expect_success 'conflict markers contain ours content' '
	(
	cd work &&
	grep "line 2 OURS" result.txt
	)
'

test_expect_success 'conflict markers contain theirs content' '
	(
	cd work &&
	grep "line 2 THEIRS" result.txt
	)
'

# ── --ours ────────────────────────────────────────────────────────────────────

test_expect_success '--ours resolves conflict to our side' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	grit merge-file --ours result.txt base.txt theirs.txt &&
	grep "line 2 OURS" result.txt &&
	! grep "line 2 THEIRS" result.txt
	)
'

test_expect_success '--ours exits 0 (no conflict)' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	grit merge-file --ours result.txt base.txt theirs.txt
	)
'

test_expect_success '--ours preserves non-conflicting lines' '
	(
	cd work &&
	grep "line 1" result.txt &&
	grep "line 3" result.txt
	)
'

test_expect_success '--ours produces no conflict markers' '
	(
	cd work &&
	! grep "^<<<<<<<" result.txt &&
	! grep "^=======" result.txt &&
	! grep "^>>>>>>>" result.txt
	)
'

# ── --theirs ──────────────────────────────────────────────────────────────────

test_expect_success '--theirs resolves conflict to their side' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	grit merge-file --theirs result.txt base.txt theirs.txt &&
	grep "line 2 THEIRS" result.txt &&
	! grep "line 2 OURS" result.txt
	)
'

test_expect_success '--theirs exits 0' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	grit merge-file --theirs result.txt base.txt theirs.txt
	)
'

test_expect_success '--theirs produces no conflict markers' '
	(
	cd work &&
	! grep "^<<<<<<<" result.txt &&
	! grep "^>>>>>>>" result.txt
	)
'

# ── --union ───────────────────────────────────────────────────────────────────

test_expect_success '--union includes both sides' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	grit merge-file --union result.txt base.txt theirs.txt &&
	grep "line 2 OURS" result.txt &&
	grep "line 2 THEIRS" result.txt
	)
'

test_expect_success '--union exits 0' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	grit merge-file --union result.txt base.txt theirs.txt
	)
'

test_expect_success '--union produces no conflict markers' '
	(
	cd work &&
	! grep "^<<<<<<<" result.txt &&
	! grep "^>>>>>>>" result.txt
	)
'

test_expect_success '--union preserves non-conflicting lines' '
	(
	cd work &&
	grep "line 1" result.txt &&
	grep "line 3" result.txt
	)
'

# ── --stdout (-p) ────────────────────────────────────────────────────────────

test_expect_success '--stdout sends output to stdout' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file -p result.txt base.txt theirs.txt >stdout_out &&
	grep "<<<<<<<" stdout_out
	)
'

test_expect_success '--stdout does not modify original file' '
	(
	cd work &&
	test_cmp ours.txt result.txt
	)
'

test_expect_success '--stdout --ours sends resolved output to stdout' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	grit merge-file -p --ours result.txt base.txt theirs.txt >stdout_out &&
	grep "line 2 OURS" stdout_out &&
	! grep "line 2 THEIRS" stdout_out
	)
'

test_expect_success '--stdout --theirs sends resolved output to stdout' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	grit merge-file -p --theirs result.txt base.txt theirs.txt >stdout_out &&
	grep "line 2 THEIRS" stdout_out
	)
'

# ── --diff3 ──────────────────────────────────────────────────────────────────

test_expect_success '--diff3 shows base in conflict markers' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file --diff3 result.txt base.txt theirs.txt &&
	grep "^|||||||" result.txt &&
	grep "line 2$" result.txt
	)
'

test_expect_success '--diff3 still shows ours and theirs sections' '
	(
	cd work &&
	grep "line 2 OURS" result.txt &&
	grep "line 2 THEIRS" result.txt
	)
'

# ── --zdiff3 ─────────────────────────────────────────────────────────────────

test_expect_success '--zdiff3 shows base section in conflict' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file --zdiff3 result.txt base.txt theirs.txt &&
	grep "^|||||||" result.txt
	)
'

# ── --quiet ───────────────────────────────────────────────────────────────────

test_expect_success '--quiet suppresses conflict warning' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file --quiet result.txt base.txt theirs.txt 2>err &&
	test_must_be_empty err
	)
'

test_expect_success '--quiet still produces conflict markers' '
	(
	cd work &&
	grep "^<<<<<<<" result.txt
	)
'

test_expect_success '--quiet on clean merge produces no output' '
	(
	cd work &&
	make_clean &&
	cp ours.txt result.txt &&
	grit merge-file --quiet result.txt base.txt theirs.txt 2>err &&
	test_must_be_empty err
	)
'

# ── -L labels ─────────────────────────────────────────────────────────────────

test_expect_success '-L sets custom labels in markers' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file -L "MINE" -L "BASE" -L "YOURS" result.txt base.txt theirs.txt &&
	grep "^<<<<<<< MINE" result.txt &&
	grep "^>>>>>>> YOURS" result.txt
	)
'

test_expect_success '-L with two labels' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file -L "left" -L "center" result.txt base.txt theirs.txt &&
	grep "^<<<<<<< left" result.txt
	)
'

# ── --marker-size ────────────────────────────────────────────────────────────

test_expect_success '--marker-size changes conflict marker width' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file --marker-size 10 result.txt base.txt theirs.txt &&
	grep "^<<<<<<<<<<" result.txt &&
	grep "^>>>>>>>>>>" result.txt
	)
'

test_expect_success '--marker-size with --diff3 affects all markers' '
	(
	cd work &&
	make_conflict &&
	cp ours.txt result.txt &&
	test_must_fail grit merge-file --marker-size 10 --diff3 result.txt base.txt theirs.txt &&
	grep "^<<<<<<<<<<" result.txt &&
	grep "^||||||||||" result.txt &&
	grep "^==========" result.txt &&
	grep "^>>>>>>>>>>" result.txt
	)
'

# ── missing files ─────────────────────────────────────────────────────────────

test_expect_success 'merge-file with nonexistent file fails' '
	(
	cd work &&
	echo "content" >exists.txt &&
	test_must_fail grit merge-file exists.txt nonexistent.txt exists.txt 2>err &&
	test -s err
	)
'

# ── identical files (no change) ──────────────────────────────────────────────

test_expect_success 'merge identical files exits 0' '
	(
	cd work &&
	cat >same.txt <<-\EOF &&
	identical content
	EOF
	cp same.txt ours_same.txt &&
	cp same.txt theirs_same.txt &&
	grit merge-file ours_same.txt same.txt theirs_same.txt &&
	test_cmp same.txt ours_same.txt
	)
'

test_expect_success 'merge with one side identical to base' '
	(
	cd work &&
	cat >b.txt <<-\EOF &&
	one
	two
	three
	EOF
	cp b.txt o.txt &&
	cat >t.txt <<-\EOF &&
	one
	two changed
	three
	EOF
	grit merge-file o.txt b.txt t.txt &&
	grep "two changed" o.txt
	)
'

test_done
