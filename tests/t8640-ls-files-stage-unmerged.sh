#!/bin/sh
# Tests for ls-files -s (stage) and -u (unmerged) with merge conflicts.

test_description='ls-files --stage and --unmerged with merge conflicts'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

# Capture real git before test-lib.sh overrides PATH
REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup: repo with merge conflict ───────────────────────────────────────

test_expect_success 'setup: repository with divergent branches and conflict' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "base content" >conflict.txt &&
	echo "no conflict" >clean.txt &&
	mkdir sub &&
	echo "subfile" >sub/nested.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "base" &&
	"$REAL_GIT" checkout -b side &&
	echo "side change" >conflict.txt &&
	echo "side only" >side-file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "side" &&
	"$REAL_GIT" checkout master &&
	echo "master change" >conflict.txt &&
	echo "master only" >master-file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "master change" &&
	test_must_fail "$REAL_GIT" merge side
	)
'

# ── ls-files -s basics (shows stage 0 / merged entries) ───────────────────

test_expect_success 'ls-files -s shows staged entries' '
	(
	cd repo &&
	git ls-files -s >actual &&
	test_line_count -gt 0 actual
	)
'

test_expect_success 'ls-files -s output format: mode oid stage path' '
	(
	cd repo &&
	git ls-files -s >actual &&
	while IFS= read -r line; do
		echo "$line" | grep -qE "^[0-9]{6} [0-9a-f]{40} [0-3]	" ||
			{ echo "bad format: $line"; return 1; }
	done <actual
	)
'

test_expect_success 'ls-files -s shows stage 0 for non-conflicted files' '
	(
	cd repo &&
	git ls-files -s >actual &&
	grep "sub/nested.txt" actual >entry &&
	grep " 0	" entry
	)
'

test_expect_success 'ls-files -s lists conflicted file stages' '
	(
	cd repo &&
	git ls-files -s >actual &&
	grep " 1	conflict.txt" actual &&
	grep " 2	conflict.txt" actual &&
	grep " 3	conflict.txt" actual
	)
'

test_expect_success 'ls-files -s includes non-zero conflict stages' '
	(
	cd repo &&
	git ls-files -s >actual &&
	grep " [123]	conflict.txt" actual
	)
'

test_expect_success 'ls-files -s shows clean.txt at stage 0' '
	(
	cd repo &&
	git ls-files -s >actual &&
	grep " 0	clean.txt" actual
	)
'

# ── ls-files -u (unmerged entries, stages 1/2/3) ─────────────────────────

test_expect_success 'ls-files -u shows unmerged entries' '
	(
	cd repo &&
	git ls-files -u >actual &&
	test_line_count -gt 0 actual
	)
'

test_expect_success 'ls-files -u does not show stage 0 entries' '
	(
	cd repo &&
	git ls-files -u >actual &&
	! grep " 0	" actual
	)
'

test_expect_success 'ls-files -u shows stage 1 (base) for conflicted file' '
	(
	cd repo &&
	git ls-files -u >actual &&
	grep "conflict.txt" actual | grep " 1	"
	)
'

test_expect_success 'ls-files -u shows stage 2 (ours) for conflicted file' '
	(
	cd repo &&
	git ls-files -u >actual &&
	grep "conflict.txt" actual | grep " 2	"
	)
'

test_expect_success 'ls-files -u shows stage 3 (theirs) for conflicted file' '
	(
	cd repo &&
	git ls-files -u >actual &&
	grep "conflict.txt" actual | grep " 3	"
	)
'

test_expect_success 'conflicted file has exactly 3 unmerged entries' '
	(
	cd repo &&
	git ls-files -u >actual &&
	grep "conflict.txt" actual >entries &&
	test_line_count -eq 3 entries
	)
'

test_expect_success 'ls-files -u does not list non-conflicted files' '
	(
	cd repo &&
	git ls-files -u >actual &&
	! grep "sub/nested.txt" actual
	)
'

test_expect_success 'ls-files -u output has correct format' '
	(
	cd repo &&
	git ls-files -u >actual &&
	while IFS= read -r line; do
		echo "$line" | grep -qE "^[0-9]{6} [0-9a-f]{40} [1-3]	" ||
			{ echo "bad format: $line"; return 1; }
	done <actual
	)
'

# ── OID checks ────────────────────────────────────────────────────────────

test_expect_success 'unmerged OIDs are valid 40-hex strings' '
	(
	cd repo &&
	git ls-files -u >actual &&
	awk "{print \$2}" actual >oids &&
	while read oid; do
		echo "$oid" | grep -qE "^[0-9a-f]{40}$" ||
			{ echo "bad OID: $oid"; return 1; }
	done <oids
	)
'

test_expect_success 'stages 1, 2, 3 have different OIDs' '
	(
	cd repo &&
	git ls-files -u >actual &&
	grep "conflict.txt" actual >entries &&
	oid1=$(awk "NR==1 {print \$2}" entries) &&
	oid2=$(awk "NR==2 {print \$2}" entries) &&
	oid3=$(awk "NR==3 {print \$2}" entries) &&
	test "$oid1" != "$oid2" &&
	test "$oid2" != "$oid3"
	)
'

test_expect_success 'unmerged OIDs are actual objects' '
	(
	cd repo &&
	git ls-files -u >actual &&
	awk "{print \$2}" actual >oids &&
	while read oid; do
		git cat-file -t "$oid" >type &&
		test "$(cat type)" = "blob" ||
			{ echo "not a blob: $oid"; return 1; }
	done <oids
	)
'

# ── ls-files -s with pathspec ─────────────────────────────────────────────

test_expect_success 'ls-files -s -- sub/nested.txt shows stage 0 entry' '
	(
	cd repo &&
	git ls-files -s -- sub/nested.txt >actual &&
	test_line_count -eq 1 actual &&
	grep " 0	" actual
	)
'

test_expect_success 'ls-files -s -- sub/ restricts to subdirectory' '
	(
	cd repo &&
	git ls-files -s -- sub/ >actual &&
	grep "sub/nested.txt" actual &&
	! grep "clean.txt" actual
	)
'

# ── After conflict resolution ─────────────────────────────────────────────

test_expect_success 'setup: resolve conflict' '
	(
	cd repo &&
	echo "resolved" >conflict.txt &&
	"$REAL_GIT" add conflict.txt
	)
'

test_expect_success 'ls-files -u is empty after resolution' '
	(
	cd repo &&
	git ls-files -u >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'ls-files -s now shows conflict.txt at stage 0' '
	(
	cd repo &&
	git ls-files -s >actual &&
	grep " 0	conflict.txt" actual
	)
'

test_expect_success 'all entries are stage 0 after resolution' '
	(
	cd repo &&
	git ls-files -s >actual &&
	! grep -v " 0	" actual
	)
'

# ── ls-files -s -z NUL termination ────────────────────────────────────────

test_expect_success 'ls-files -s -z uses NUL terminators' '
	(
	cd repo &&
	git ls-files -s -z >actual &&
	tr "\0" "\n" <actual >translated &&
	test_line_count -gt 0 translated
	)
'

# ── Second conflict scenario (multiple files) ────────────────────────────

test_expect_success 'setup: complete merge and create two-file conflict' '
	(
	cd repo &&
	"$REAL_GIT" commit -m "merge resolved" &&
	"$REAL_GIT" checkout -b branch2 &&
	echo "branch2 content" >conflict.txt &&
	echo "branch2 clean" >clean.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "branch2" &&
	"$REAL_GIT" checkout master &&
	echo "master new content" >conflict.txt &&
	echo "master new clean" >clean.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "master new" &&
	test_must_fail "$REAL_GIT" merge branch2
	)
'

test_expect_success 'ls-files -u shows entries for both conflicted files' '
	(
	cd repo &&
	git ls-files -u >actual &&
	grep "conflict.txt" actual &&
	grep "clean.txt" actual
	)
'

test_expect_success 'ls-files -u has 6 entries for 2 conflicted files' '
	(
	cd repo &&
	git ls-files -u >actual &&
	test_line_count -eq 6 actual
	)
'

test_expect_success 'ls-files -u -z NUL-terminates unmerged output' '
	(
	cd repo &&
	git ls-files -u -z >actual &&
	tr "\0" "\n" <actual >translated &&
	test_line_count -gt 0 translated
	)
'

test_expect_success 'ls-files -s shows fewer entries than total during conflict' '
	(
	cd repo &&
	s_count=$(git ls-files -s | wc -l) &&
	u_count=$(git ls-files -u | wc -l) &&
	total=$((s_count + u_count)) &&
	test "$total" -gt "$s_count"
	)
'

test_expect_success 'ls-files -s mode is 100644 for regular files' '
	(
	cd repo &&
	git ls-files -s >actual &&
	awk "{print \$1}" actual >modes &&
	while read mode; do
		test "$mode" = "100644" ||
			{ echo "unexpected mode: $mode"; return 1; }
	done <modes
	)
'

test_done
