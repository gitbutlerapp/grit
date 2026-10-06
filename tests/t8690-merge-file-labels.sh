#!/bin/sh
# Tests for merge-file with -L labels, --marker-size, --diff3, and conflict styles.

test_description='merge-file labels, marker-size, diff3'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# ── helpers ───────────────────────────────────────────────────────────────────

# Create three files for merge-file: base, ours, theirs
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
	ours change
	line 3
	line 4
	line 5
	EOF
	cat >theirs.txt <<-\EOF
	line 1
	theirs change
	line 3
	line 4
	line 5
	EOF
}

setup_no_conflict () {
	cat >base.txt <<-\EOF &&
	line 1
	line 2
	line 3
	line 4
	line 5
	EOF
	cat >ours.txt <<-\EOF &&
	line 1
	ours addition
	line 2
	line 3
	line 4
	line 5
	EOF
	cat >theirs.txt <<-\EOF
	line 1
	line 2
	line 3
	line 4
	theirs addition
	line 5
	EOF
}

# ── basic merge-file tests ───────────────────────────────────────────────────

test_expect_success 'merge-file with no conflict returns 0' '
	setup_no_conflict &&
	cp ours.txt result.txt &&
	git merge-file result.txt base.txt theirs.txt
'

test_expect_success 'merge-file with conflict returns non-zero' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file result.txt base.txt theirs.txt
'

test_expect_success 'merge-file conflict markers use 7 chars by default' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file result.txt base.txt theirs.txt &&
	grep "^<<<<<<< " result.txt &&
	grep "^=======$" result.txt &&
	grep "^>>>>>>> " result.txt
'

test_expect_success 'merge-file -p sends output to stdout' '
	setup_conflict &&
	cp ours.txt save.txt &&
	test_must_fail git merge-file -p ours.txt base.txt theirs.txt >out.txt &&
	test_cmp save.txt ours.txt &&
	grep "<<<<<<< " out.txt
'

# ── -L label tests ───────────────────────────────────────────────────────────

test_expect_success 'merge-file -L sets ours label' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file -p -L "OUR_VERSION" result.txt base.txt theirs.txt >out.txt &&
	grep "^<<<<<<< OUR_VERSION$" out.txt
'

test_expect_success 'merge-file -L -L sets ours and base labels' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file -p -L "OURS" -L "BASE" result.txt base.txt theirs.txt >out.txt &&
	grep "^<<<<<<< OURS$" out.txt
'

test_expect_success 'merge-file -L -L -L sets all three labels' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file -p \
		-L "MY_FILE" -L "ANCESTOR" -L "THEIR_FILE" \
		result.txt base.txt theirs.txt >out.txt &&
	grep "^<<<<<<< MY_FILE$" out.txt &&
	grep "^>>>>>>> THEIR_FILE$" out.txt
'

test_expect_success 'merge-file labels with spaces work' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file -p \
		-L "my file.txt" -L "base version" -L "their file.txt" \
		result.txt base.txt theirs.txt >out.txt &&
	grep "^<<<<<<< my file.txt$" out.txt &&
	grep "^>>>>>>> their file.txt$" out.txt
'

test_expect_success 'merge-file labels with special characters' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file -p \
		-L "a/b/c.txt" -L "orig" -L "d/e/f.txt" \
		result.txt base.txt theirs.txt >out.txt &&
	grep "^<<<<<<< a/b/c.txt$" out.txt &&
	grep "^>>>>>>> d/e/f.txt$" out.txt
'

test_expect_success 'merge-file label defaults to filename when no -L' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file -p result.txt base.txt theirs.txt >out.txt &&
	grep "^<<<<<<< result.txt$" out.txt &&
	grep "^>>>>>>> theirs.txt$" out.txt
'

# ── --marker-size tests ──────────────────────────────────────────────────────

test_expect_success 'merge-file --marker-size 10 produces 10-char markers' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file -p --marker-size 10 result.txt base.txt theirs.txt >out.txt &&
	grep "^<<<<<<<<<< " out.txt &&
	grep "^==========$" out.txt &&
	grep "^>>>>>>>>>> " out.txt
'

test_expect_success 'merge-file --marker-size 3 produces 3-char markers' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file -p --marker-size 3 result.txt base.txt theirs.txt >out.txt &&
	grep "^<<< " out.txt &&
	grep "^===$" out.txt &&
	grep "^>>> " out.txt
'

test_expect_success 'merge-file --marker-size with -L labels' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file -p \
		--marker-size 9 -L "OURS" -L "BASE" -L "THEIRS" \
		result.txt base.txt theirs.txt >out.txt &&
	grep "^<<<<<<<<< OURS$" out.txt &&
	grep "^>>>>>>>>> THEIRS$" out.txt
'

# ── --diff3 tests ────────────────────────────────────────────────────────────

test_expect_success 'merge-file --diff3 shows base version in conflict' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file -p --diff3 result.txt base.txt theirs.txt >out.txt &&
	grep "^<<<<<<< " out.txt &&
	grep "^||||||| " out.txt &&
	grep "^=======$" out.txt &&
	grep "^>>>>>>> " out.txt
'

test_expect_success 'merge-file --diff3 includes original base text' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file -p --diff3 result.txt base.txt theirs.txt >out.txt &&
	# Between ||||||| and ======= we should see the base content
	sed -n "/^|||||||/,/^=======/p" out.txt >base_section.txt &&
	grep "line 2" base_section.txt
'

test_expect_success 'merge-file --diff3 with -L labels shows all three' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file -p --diff3 \
		-L "OURS" -L "BASE" -L "THEIRS" \
		result.txt base.txt theirs.txt >out.txt &&
	grep "^<<<<<<< OURS$" out.txt &&
	grep "^||||||| BASE$" out.txt &&
	grep "^>>>>>>> THEIRS$" out.txt
'

test_expect_success 'merge-file --diff3 with --marker-size' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file -p --diff3 --marker-size 10 \
		result.txt base.txt theirs.txt >out.txt &&
	grep "^<<<<<<<<<< " out.txt &&
	grep "^|||||||||| " out.txt &&
	grep "^==========$" out.txt &&
	grep "^>>>>>>>>>> " out.txt
'

# ── --zdiff3 tests ───────────────────────────────────────────────────────────

test_expect_success 'merge-file --zdiff3 shows base like diff3' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file -p --zdiff3 result.txt base.txt theirs.txt >out.txt &&
	grep "^||||||| " out.txt
'

# ── --ours / --theirs / --union ──────────────────────────────────────────────

test_expect_success 'merge-file --ours resolves conflict with ours' '
	setup_conflict &&
	cp ours.txt result.txt &&
	git merge-file --ours result.txt base.txt theirs.txt &&
	grep "ours change" result.txt &&
	! grep "theirs change" result.txt
'

test_expect_success 'merge-file --theirs resolves conflict with theirs' '
	setup_conflict &&
	cp ours.txt result.txt &&
	git merge-file --theirs result.txt base.txt theirs.txt &&
	grep "theirs change" result.txt &&
	! grep "ours change" result.txt
'

test_expect_success 'merge-file --union includes both sides' '
	setup_conflict &&
	cp ours.txt result.txt &&
	git merge-file --union result.txt base.txt theirs.txt &&
	grep "ours change" result.txt &&
	grep "theirs change" result.txt &&
	! grep "<<<<<<< " result.txt
'

# ── quiet mode ───────────────────────────────────────────────────────────────

test_expect_success 'merge-file -q suppresses conflict warnings' '
	setup_conflict &&
	cp ours.txt result.txt &&
	test_must_fail git merge-file -q result.txt base.txt theirs.txt 2>err.txt &&
	test_must_be_empty err.txt
'

# ── multi-region conflicts ───────────────────────────────────────────────────

test_expect_success 'merge-file with multiple conflict regions' '
	cat >base_multi.txt <<-\EOF &&
	alpha
	beta
	gamma
	delta
	epsilon
	EOF
	cat >ours_multi.txt <<-\EOF &&
	alpha-ours
	beta
	gamma
	delta-ours
	epsilon
	EOF
	cat >theirs_multi.txt <<-\EOF &&
	alpha-theirs
	beta
	gamma
	delta-theirs
	epsilon
	EOF
	cp ours_multi.txt result_multi.txt &&
	test_must_fail git merge-file -p result_multi.txt base_multi.txt theirs_multi.txt >out.txt &&
	# Adjacent edits coalesce into one conflict block
	count=$(grep -c "^<<<<<<< " out.txt) &&
	test "$count" -eq 1
'

test_expect_success 'merge-file --diff3 with multiple conflict regions shows base for each' '
	cat >base_multi.txt <<-\EOF &&
	alpha
	beta
	gamma
	delta
	epsilon
	EOF
	cat >ours_multi.txt <<-\EOF &&
	alpha-ours
	beta
	gamma
	delta-ours
	epsilon
	EOF
	cat >theirs_multi.txt <<-\EOF &&
	alpha-theirs
	beta
	gamma
	delta-theirs
	epsilon
	EOF
	cp ours_multi.txt result_multi.txt &&
	test_must_fail git merge-file -p --diff3 result_multi.txt base_multi.txt theirs_multi.txt >out.txt &&
	count=$(grep -c "^||||||| " out.txt) &&
	test "$count" -eq 2
'

# ── identical inputs ─────────────────────────────────────────────────────────

test_expect_success 'merge-file with identical ours and theirs returns clean' '
	cat >base_id.txt <<-\EOF &&
	original
	EOF
	cat >same.txt <<-\EOF &&
	changed
	EOF
	cp same.txt ours_id.txt &&
	cp same.txt theirs_id.txt &&
	cp ours_id.txt result_id.txt &&
	git merge-file result_id.txt base_id.txt theirs_id.txt &&
	echo "changed" >expect &&
	test_cmp expect result_id.txt
'

test_expect_success 'merge-file with all three identical returns clean' '
	echo "same content" >a.txt &&
	echo "same content" >b.txt &&
	echo "same content" >c.txt &&
	cp a.txt r.txt &&
	git merge-file r.txt b.txt c.txt &&
	echo "same content" >expect &&
	test_cmp expect r.txt
'

# ── empty files ──────────────────────────────────────────────────────────────

test_expect_success 'merge-file with empty base and different additions produces conflict' '
	>empty_base.txt &&
	echo "ours content" >ours_e.txt &&
	echo "theirs content" >theirs_e.txt &&
	cp ours_e.txt result_e.txt &&
	test_must_fail git merge-file -p result_e.txt empty_base.txt theirs_e.txt >out.txt &&
	grep "ours content" out.txt &&
	grep "theirs content" out.txt
'

test_expect_success 'merge-file with all empty files returns clean' '
	>e1.txt && >e2.txt && >e3.txt &&
	cp e1.txt r_e.txt &&
	git merge-file r_e.txt e2.txt e3.txt &&
	test_must_be_empty r_e.txt
'

test_done
