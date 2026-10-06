#!/bin/sh
# Test diff-tree with commit ranges, various output formats,
# single-commit mode, and option combinations.
#
# History: C1(initial) -> C2(mod a) -> C3(add c) -> C4(mod b, del a) -> C5(mod sub/s)
# HEAD=C5, HEAD~1=C4, HEAD~2=C3, HEAD~3=C2, HEAD~4=C1

test_description='grit diff-tree commit range'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---- setup ----

test_expect_success 'setup: create repo with linear history' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "alpha" >a.txt &&
	echo "bravo" >b.txt &&
	mkdir -p sub &&
	echo "sub content" >sub/s.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "C1: initial" &&
	echo "alpha2" >>a.txt &&
	grit add a.txt &&
	test_tick &&
	grit commit -m "C2: modify a.txt" &&
	echo "charlie" >c.txt &&
	grit add c.txt &&
	test_tick &&
	grit commit -m "C3: add c.txt" &&
	echo "bravo2" >>b.txt &&
	rm a.txt &&
	grit add b.txt a.txt &&
	test_tick &&
	grit commit -m "C4: modify b.txt delete a.txt" &&
	echo "sub2" >>sub/s.txt &&
	grit add sub/s.txt &&
	test_tick &&
	grit commit -m "C5: update sub/s.txt"
	)
'

# ---- basic two-commit diff-tree ----

test_expect_success 'diff-tree two commits shows raw output' '
	(
	cd repo &&
	grit diff-tree HEAD~1 HEAD >actual &&
	test -s actual
	)
'

test_expect_success 'diff-tree HEAD~1 HEAD has colon-prefixed lines' '
	(
	cd repo &&
	grit diff-tree HEAD~1 HEAD >actual &&
	grep "^:" actual
	)
'

test_expect_success 'diff-tree C1->C2 shows a.txt modification' '
	(
	cd repo &&
	grit diff-tree HEAD~4 HEAD~3 >actual &&
	grep "M" actual &&
	grep "a.txt" actual
	)
'

test_expect_success 'diff-tree C2->C3 shows c.txt addition' '
	(
	cd repo &&
	grit diff-tree HEAD~3 HEAD~2 >actual &&
	grep "A" actual &&
	grep "c.txt" actual
	)
'

test_expect_success 'diff-tree C3->C4 shows deletion and modification' '
	(
	cd repo &&
	grit diff-tree HEAD~2 HEAD~1 >actual &&
	grep "a.txt" actual &&
	grep "b.txt" actual
	)
'

# ---- single-commit mode ----

test_expect_success 'diff-tree single commit shows changes vs parent' '
	(
	cd repo &&
	grit diff-tree HEAD >actual &&
	test -s actual
	)
'

test_expect_success 'diff-tree single commit for C5 shows sub dir change' '
	(
	cd repo &&
	grit diff-tree HEAD >actual &&
	grep "sub" actual
	)
'

test_expect_success 'diff-tree single commit for root commit' '
	(
	cd repo &&
	first=$(grit rev-list HEAD | tail -1) &&
	grit diff-tree "$first" >actual &&
	# root commit - may or may not produce output
	true
	)
'

# ---- --name-only ----

test_expect_success 'diff-tree --name-only shows only filenames' '
	(
	cd repo &&
	grit diff-tree --name-only HEAD~4 HEAD~3 >actual &&
	grep "a.txt" actual &&
	! grep "^:" actual
	)
'

test_expect_success 'diff-tree --name-only HEAD~4 HEAD shows multiple entries' '
	(
	cd repo &&
	grit diff-tree --name-only HEAD~4 HEAD >actual &&
	line_count=$(wc -l <actual | tr -d " ") &&
	test "$line_count" -ge 2
	)
'

test_expect_success 'diff-tree --name-only has no mode/hash info' '
	(
	cd repo &&
	grit diff-tree --name-only HEAD~4 HEAD~3 >actual &&
	! grep -E "^[0-9]{6}" actual
	)
'

test_expect_success 'diff-tree -r --name-only shows files in subdirs' '
	(
	cd repo &&
	grit diff-tree -r --name-only HEAD~1 HEAD >actual &&
	grep "sub/s.txt" actual
	)
'

# ---- --name-status ----

test_expect_success 'diff-tree --name-status C1->C2 shows M for a.txt' '
	(
	cd repo &&
	grit diff-tree --name-status HEAD~4 HEAD~3 >actual &&
	grep "^M" actual &&
	grep "a.txt" actual
	)
'

test_expect_success 'diff-tree --name-status C2->C3 shows A for c.txt' '
	(
	cd repo &&
	grit diff-tree --name-status HEAD~3 HEAD~2 >actual &&
	grep "^A" actual &&
	grep "c.txt" actual
	)
'

test_expect_success 'diff-tree --name-status C3->C4 shows D for a.txt' '
	(
	cd repo &&
	grit diff-tree --name-status HEAD~2 HEAD~1 >actual &&
	grep "D" actual &&
	grep "a.txt" actual
	)
'

# ---- --stat ----

test_expect_success 'diff-tree --stat C1->C2 shows a.txt' '
	(
	cd repo &&
	grit diff-tree --stat HEAD~4 HEAD~3 >actual &&
	grep "a.txt" actual
	)
'

test_expect_success 'diff-tree --stat shows summary line' '
	(
	cd repo &&
	grit diff-tree --stat HEAD~4 HEAD~3 >actual &&
	grep "file.*changed" actual
	)
'

test_expect_success 'diff-tree --stat wider range' '
	(
	cd repo &&
	grit diff-tree --stat HEAD~4 HEAD >actual &&
	line_count=$(wc -l <actual | tr -d " ") &&
	test "$line_count" -ge 2
	)
'

# ---- -p (patch) ----

test_expect_success 'diff-tree -p shows unified diff' '
	(
	cd repo &&
	grit diff-tree -p HEAD~4 HEAD~3 >actual &&
	grep "^diff --git" actual
	)
'

test_expect_success 'diff-tree -p shows addition lines' '
	(
	cd repo &&
	grit diff-tree -p HEAD~4 HEAD~3 >actual &&
	grep "^+" actual
	)
'

test_expect_success 'diff-tree -p shows correct file in header' '
	(
	cd repo &&
	grit diff-tree -p HEAD~4 HEAD~3 >actual &&
	grep "a.txt" actual
	)
'

# ---- -r (recursive) ----

test_expect_success 'diff-tree -r shows files in subdirectories' '
	(
	cd repo &&
	grit diff-tree -r HEAD~1 HEAD >actual &&
	grep "sub/s.txt" actual
	)
'

test_expect_success 'diff-tree -r wider range includes subdirectory files' '
	(
	cd repo &&
	grit diff-tree -r HEAD~4 HEAD >actual &&
	grep "sub/s.txt" actual
	)
'

test_expect_success 'diff-tree without -r shows tree-level change for sub' '
	(
	cd repo &&
	grit diff-tree HEAD~1 HEAD >actual &&
	grep "sub" actual &&
	! grep "sub/s.txt" actual
	)
'

# ---- combinations ----

test_expect_success 'diff-tree -r --name-only full range' '
	(
	cd repo &&
	grit diff-tree -r --name-only HEAD~4 HEAD >actual &&
	grep "sub/s.txt" actual &&
	! grep "^:" actual
	)
'

test_expect_success 'diff-tree same commit shows no changes' '
	(
	cd repo &&
	grit diff-tree HEAD HEAD >actual &&
	test ! -s actual
	)
'

# ---- rev specification ----

test_expect_success 'diff-tree with explicit SHA works' '
	(
	cd repo &&
	sha1=$(grit rev-list HEAD | sed -n 2p) &&
	sha2=$(grit rev-list HEAD | sed -n 1p) &&
	grit diff-tree "$sha1" "$sha2" >actual &&
	test -s actual
	)
'

test_expect_success 'diff-tree HEAD~N notation works' '
	(
	cd repo &&
	grit diff-tree HEAD~4 HEAD~3 >actual &&
	grep "a.txt" actual
	)
'

test_expect_success 'diff-tree HEAD~4 HEAD shows accumulated changes' '
	(
	cd repo &&
	grit diff-tree --name-only HEAD~4 HEAD >actual &&
	line_count=$(wc -l <actual | tr -d " ") &&
	test "$line_count" -ge 3
	)
'

# ---- output format consistency ----

test_expect_success 'diff-tree raw output has 40-char hashes' '
	(
	cd repo &&
	grit diff-tree HEAD~4 HEAD~3 >actual &&
	grep -E "[0-9a-f]{40}" actual
	)
'

test_expect_success 'diff-tree raw lines have tab before filename' '
	(
	cd repo &&
	grit diff-tree HEAD~4 HEAD~3 >actual &&
	grep "	a.txt" actual
	)
'

test_expect_success 'diff-tree --name-status lines are status<TAB>name' '
	(
	cd repo &&
	grit diff-tree --name-status HEAD~4 HEAD~3 >actual &&
	grep -E "^[ADMR]	" actual
	)
'

test_done
