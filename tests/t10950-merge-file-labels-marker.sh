#!/bin/sh
# Tests for merge-file: labels (-L), marker-size, conflict styles,
# --ours/--theirs/--union, --stdout, --diff3, --zdiff3, --quiet.

test_description='merge-file labels, marker size, and conflict options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup helpers ────────────────────────────────────────────────────────────

# Create three files: base, ours, theirs with a clean merge
write_clean_merge () {
	cat >base.txt <<-\EOF &&
	line1
	line2
	line3
	line4
	line5
	EOF
	cat >ours.txt <<-\EOF &&
	line1
	line2-ours
	line3
	line4
	line5
	EOF
	cat >theirs.txt <<-\EOF
	line1
	line2
	line3
	line4-theirs
	line5
	EOF
}

# Create three files with a conflict on the same line
write_conflict_merge () {
	cat >base.txt <<-\EOF &&
	aaa
	bbb
	ccc
	EOF
	cat >ours.txt <<-\EOF &&
	aaa
	ours-change
	ccc
	EOF
	cat >theirs.txt <<-\EOF
	aaa
	theirs-change
	ccc
	EOF
}

# ── Basic clean merge ────────────────────────────────────────────────────────

test_expect_success 'clean merge with no conflicts exits 0' '
	write_clean_merge &&
	cp ours.txt current.txt &&
	grit merge-file current.txt base.txt theirs.txt &&
	grep "line2-ours" current.txt &&
	grep "line4-theirs" current.txt
'

test_expect_success 'clean merge preserves unchanged lines' '
	write_clean_merge &&
	cp ours.txt current.txt &&
	grit merge-file current.txt base.txt theirs.txt &&
	grep "^line1$" current.txt &&
	grep "^line3$" current.txt &&
	grep "^line5$" current.txt
'

test_expect_success 'clean merge result has correct line count' '
	write_clean_merge &&
	cp ours.txt current.txt &&
	grit merge-file current.txt base.txt theirs.txt &&
	test_line_count = 5 current.txt
'

# ── Conflict merge ──────────────────────────────────────────────────────────

test_expect_success 'conflicting merge exits non-zero' '
	write_conflict_merge &&
	cp ours.txt current.txt &&
	test_must_fail grit merge-file current.txt base.txt theirs.txt
'

test_expect_success 'conflict markers appear in output' '
	write_conflict_merge &&
	cp ours.txt current.txt &&
	test_must_fail grit merge-file current.txt base.txt theirs.txt &&
	grep "^<<<<<<<" current.txt &&
	grep "^=======" current.txt &&
	grep "^>>>>>>>" current.txt
'

test_expect_success 'conflict includes both sides' '
	write_conflict_merge &&
	cp ours.txt current.txt &&
	test_must_fail grit merge-file current.txt base.txt theirs.txt &&
	grep "ours-change" current.txt &&
	grep "theirs-change" current.txt
'

test_expect_success 'non-conflicting lines preserved in conflict merge' '
	write_conflict_merge &&
	cp ours.txt current.txt &&
	test_must_fail grit merge-file current.txt base.txt theirs.txt &&
	grep "^aaa$" current.txt &&
	grep "^ccc$" current.txt
'

# ── --stdout / -p ───────────────────────────────────────────────────────────

test_expect_success '--stdout sends result to stdout' '
	write_clean_merge &&
	grit merge-file -p ours.txt base.txt theirs.txt >result.txt &&
	grep "line2-ours" result.txt &&
	grep "line4-theirs" result.txt
'

test_expect_success '--stdout does not modify input file' '
	write_clean_merge &&
	cp ours.txt ours_backup.txt &&
	grit merge-file --stdout ours.txt base.txt theirs.txt >result.txt &&
	test_cmp ours_backup.txt ours.txt
'

test_expect_success '--stdout with conflict still shows markers' '
	write_conflict_merge &&
	test_must_fail grit merge-file -p ours.txt base.txt theirs.txt >result.txt &&
	grep "^<<<<<<<" result.txt &&
	grep "^=======" result.txt &&
	grep "^>>>>>>>" result.txt
'

# ── Labels (-L) ─────────────────────────────────────────────────────────────

test_expect_success '-L sets ours label in conflict marker' '
	write_conflict_merge &&
	test_must_fail grit merge-file -p \
		-L "MY-VERSION" -L "BASE" -L "THEIR-VERSION" \
		ours.txt base.txt theirs.txt >result.txt &&
	grep "<<<<<<< MY-VERSION" result.txt
'

test_expect_success '-L sets theirs label in conflict marker' '
	write_conflict_merge &&
	test_must_fail grit merge-file -p \
		-L "MY-VERSION" -L "BASE" -L "THEIR-VERSION" \
		ours.txt base.txt theirs.txt >result.txt &&
	grep ">>>>>>> THEIR-VERSION" result.txt
'

test_expect_success 'default labels are filenames' '
	write_conflict_merge &&
	test_must_fail grit merge-file -p ours.txt base.txt theirs.txt >result.txt &&
	grep "<<<<<<< ours.txt" result.txt &&
	grep ">>>>>>> theirs.txt" result.txt
'

test_expect_success '-L with special characters in label' '
	write_conflict_merge &&
	test_must_fail grit merge-file -p \
		-L "feature/my-branch" -L "base" -L "main" \
		ours.txt base.txt theirs.txt >result.txt &&
	grep "<<<<<<< feature/my-branch" result.txt &&
	grep ">>>>>>> main" result.txt
'

# ── --marker-size ────────────────────────────────────────────────────────────

test_expect_success '--marker-size changes conflict marker length' '
	write_conflict_merge &&
	test_must_fail grit merge-file -p --marker-size 10 \
		ours.txt base.txt theirs.txt >result.txt &&
	grep "^<<<<<<<<<<" result.txt &&
	grep "^==========" result.txt &&
	grep "^>>>>>>>>>>" result.txt
'

test_expect_success '--marker-size 3 uses short markers' '
	write_conflict_merge &&
	test_must_fail grit merge-file -p --marker-size 3 \
		ours.txt base.txt theirs.txt >result.txt &&
	grep "^<<<" result.txt &&
	grep "^===" result.txt &&
	grep "^>>>" result.txt
'

test_expect_success 'default marker size is 7' '
	write_conflict_merge &&
	test_must_fail grit merge-file -p ours.txt base.txt theirs.txt >result.txt &&
	grep "^<<<<<<< " result.txt &&
	grep "^=======$" result.txt &&
	grep "^>>>>>>> " result.txt
'

# ── --ours ───────────────────────────────────────────────────────────────────

test_expect_success '--ours resolves conflict with our side' '
	write_conflict_merge &&
	grit merge-file -p --ours ours.txt base.txt theirs.txt >result.txt &&
	grep "ours-change" result.txt &&
	! grep "theirs-change" result.txt &&
	! grep "^<<<<<<<" result.txt
'

test_expect_success '--ours exits 0 (no conflict markers)' '
	write_conflict_merge &&
	grit merge-file -p --ours ours.txt base.txt theirs.txt >result.txt
'

# ── --theirs ─────────────────────────────────────────────────────────────────

test_expect_success '--theirs resolves conflict with their side' '
	write_conflict_merge &&
	grit merge-file -p --theirs ours.txt base.txt theirs.txt >result.txt &&
	grep "theirs-change" result.txt &&
	! grep "ours-change" result.txt &&
	! grep "^<<<<<<<" result.txt
'

test_expect_success '--theirs exits 0 (no conflict markers)' '
	write_conflict_merge &&
	grit merge-file -p --theirs ours.txt base.txt theirs.txt >result.txt
'

# ── --union ──────────────────────────────────────────────────────────────────

test_expect_success '--union includes both sides without markers' '
	write_conflict_merge &&
	grit merge-file -p --union ours.txt base.txt theirs.txt >result.txt &&
	grep "ours-change" result.txt &&
	grep "theirs-change" result.txt &&
	! grep "^<<<<<<<" result.txt
'

test_expect_success '--union exits 0' '
	write_conflict_merge &&
	grit merge-file -p --union ours.txt base.txt theirs.txt >result.txt
'

# ── --diff3 ──────────────────────────────────────────────────────────────────

test_expect_success '--diff3 shows base version in conflict' '
	write_conflict_merge &&
	test_must_fail grit merge-file -p --diff3 \
		ours.txt base.txt theirs.txt >result.txt &&
	grep "^|||||||" result.txt &&
	grep "bbb" result.txt
'

test_expect_success '--diff3 still has ours and theirs' '
	write_conflict_merge &&
	test_must_fail grit merge-file -p --diff3 \
		ours.txt base.txt theirs.txt >result.txt &&
	grep "ours-change" result.txt &&
	grep "theirs-change" result.txt
'

# ── --quiet ──────────────────────────────────────────────────────────────────

test_expect_success '--quiet suppresses conflict warning' '
	write_conflict_merge &&
	test_must_fail grit merge-file -p --quiet \
		ours.txt base.txt theirs.txt >result.txt 2>err &&
	test_must_be_empty err
'

# ── Identical files ──────────────────────────────────────────────────────────

test_expect_success 'merge of identical files is clean' '
	cat >same.txt <<-\EOF &&
	identical content
	EOF
	cp same.txt same2.txt &&
	cp same.txt same3.txt &&
	grit merge-file -p same.txt same2.txt same3.txt >result.txt &&
	echo "identical content" >expect &&
	test_cmp expect result.txt
'

test_expect_success 'merge where only ours changed' '
	cat >base_only.txt <<-\EOF &&
	original
	EOF
	cat >ours_only.txt <<-\EOF &&
	modified-ours
	EOF
	cp base_only.txt theirs_only.txt &&
	grit merge-file -p ours_only.txt base_only.txt theirs_only.txt >result.txt &&
	echo "modified-ours" >expect &&
	test_cmp expect result.txt
'

test_expect_success 'merge where only theirs changed' '
	cat >base_t.txt <<-\EOF &&
	original
	EOF
	cp base_t.txt ours_t.txt &&
	cat >theirs_t.txt <<-\EOF &&
	modified-theirs
	EOF
	grit merge-file -p ours_t.txt base_t.txt theirs_t.txt >result.txt &&
	echo "modified-theirs" >expect &&
	test_cmp expect result.txt
'

# ── Empty files ──────────────────────────────────────────────────────────────

test_expect_success 'merge with empty base and divergent sides produces output' '
	>empty_base.txt &&
	echo "ours-line" >ours_e.txt &&
	echo "theirs-line" >theirs_e.txt &&
	grit merge-file -p ours_e.txt empty_base.txt theirs_e.txt >result.txt || true &&
	grep "ours-line" result.txt &&
	grep "theirs-line" result.txt
'

test_expect_success 'merge of three empty files is clean' '
	>e1.txt && >e2.txt && >e3.txt &&
	grit merge-file -p e1.txt e2.txt e3.txt >result.txt &&
	test_must_be_empty result.txt
'

# ── Multi-line conflict ─────────────────────────────────────────────────────

test_expect_success 'multi-line conflict has correct structure' '
	cat >ml_base.txt <<-\EOF &&
	header
	old-line-a
	old-line-b
	footer
	EOF
	cat >ml_ours.txt <<-\EOF &&
	header
	new-ours-a
	new-ours-b
	footer
	EOF
	cat >ml_theirs.txt <<-\EOF &&
	header
	new-theirs-a
	new-theirs-b
	footer
	EOF
	test_must_fail grit merge-file -p ml_ours.txt ml_base.txt ml_theirs.txt >result.txt &&
	grep "header" result.txt &&
	grep "footer" result.txt &&
	grep "new-ours-a" result.txt &&
	grep "new-theirs-a" result.txt
'

test_done
