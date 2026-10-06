#!/bin/sh
# Test diff-index with --exit-code, --cached, --quiet, and raw output
# across various states: clean working tree, staged changes,
# unstaged changes, new files, deleted files.

test_description='grit diff-index exit code'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---- setup ----

test_expect_success 'setup: create repo with initial commit' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "line1" >file1.txt &&
	echo "line1" >file2.txt &&
	echo "line1" >file3.txt &&
	mkdir -p dir &&
	echo "nested" >dir/n.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "initial commit"
	)
'

# ---- clean state: no differences ----

test_expect_success 'diff-index HEAD exits 0 when clean (default)' '
	(
	cd repo &&
	grit diff-index HEAD >actual &&
	test ! -s actual
	)
'

test_expect_success 'diff-index --exit-code HEAD exits 0 when clean' '
	(
	cd repo &&
	grit diff-index --exit-code HEAD
	)
'

test_expect_success 'diff-index --cached HEAD exits 0 when clean' '
	(
	cd repo &&
	grit diff-index --cached HEAD >actual &&
	test ! -s actual
	)
'

test_expect_success 'diff-index --cached --exit-code HEAD exits 0 when clean' '
	(
	cd repo &&
	grit diff-index --cached --exit-code HEAD
	)
'

test_expect_success 'diff-index --quiet HEAD exits 0 when clean' '
	(
	cd repo &&
	grit diff-index --quiet HEAD
	)
'

# ---- unstaged modifications ----

test_expect_success 'setup: make unstaged modification' '
	(
	cd repo &&
	echo "line2" >>file1.txt
	)
'

test_expect_success 'diff-index HEAD shows unstaged change' '
	(
	cd repo &&
	grit diff-index HEAD >actual &&
	grep "file1.txt" actual
	)
'

test_expect_success 'diff-index HEAD default exit is 0 even with changes' '
	(
	cd repo &&
	grit diff-index HEAD >actual &&
	true
	)
'

test_expect_success 'diff-index --exit-code HEAD exits 1 with unstaged changes' '
	(
	cd repo &&
	test_must_fail grit diff-index --exit-code HEAD
	)
'

test_expect_success 'diff-index --quiet HEAD exits 1 with unstaged changes' '
	(
	cd repo &&
	test_must_fail grit diff-index --quiet HEAD
	)
'

test_expect_success 'diff-index --quiet produces no output' '
	(
	cd repo &&
	grit diff-index --quiet HEAD >actual 2>&1 || true &&
	test ! -s actual
	)
'

test_expect_success 'diff-index --cached HEAD still clean (unstaged only)' '
	(
	cd repo &&
	grit diff-index --cached HEAD >actual &&
	test ! -s actual
	)
'

test_expect_success 'diff-index --cached --exit-code exits 0 (unstaged only)' '
	(
	cd repo &&
	grit diff-index --cached --exit-code HEAD
	)
'

# ---- staged modifications ----

test_expect_success 'setup: stage the modification' '
	(
	cd repo &&
	grit add file1.txt
	)
'

test_expect_success 'diff-index --cached HEAD shows staged change' '
	(
	cd repo &&
	grit diff-index --cached HEAD >actual &&
	grep "file1.txt" actual
	)
'

test_expect_success 'diff-index --cached --exit-code exits 1 with staged changes' '
	(
	cd repo &&
	test_must_fail grit diff-index --cached --exit-code HEAD
	)
'

test_expect_success 'diff-index HEAD shows staged change too (index vs work tree includes staged)' '
	(
	cd repo &&
	grit diff-index HEAD >actual &&
	grep "file1.txt" actual
	)
'

test_expect_success 'diff-index --exit-code HEAD exits 1 with staged changes' '
	(
	cd repo &&
	test_must_fail grit diff-index --exit-code HEAD
	)
'

# ---- commit and return to clean ----

test_expect_success 'setup: commit staged change' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "commit file1 change"
	)
'

test_expect_success 'diff-index --exit-code HEAD exits 0 after commit' '
	(
	cd repo &&
	grit diff-index --exit-code HEAD
	)
'

test_expect_success 'diff-index --cached --exit-code HEAD exits 0 after commit' '
	(
	cd repo &&
	grit diff-index --cached --exit-code HEAD
	)
'

# ---- new file staged ----

test_expect_success 'setup: add new file to index' '
	(
	cd repo &&
	echo "new content" >new.txt &&
	grit add new.txt
	)
'

test_expect_success 'diff-index --cached HEAD shows new file as A' '
	(
	cd repo &&
	grit diff-index --cached HEAD >actual &&
	grep "A" actual &&
	grep "new.txt" actual
	)
'

test_expect_success 'diff-index --cached --exit-code exits 1 with new staged file' '
	(
	cd repo &&
	test_must_fail grit diff-index --cached --exit-code HEAD
	)
'

# ---- deleted file staged ----

test_expect_success 'setup: commit new file then stage deletion' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "add new.txt" &&
	rm new.txt &&
	grit add new.txt
	)
'

test_expect_success 'diff-index --cached HEAD shows deleted file as D' '
	(
	cd repo &&
	grit diff-index --cached HEAD >actual &&
	grep "D" actual &&
	grep "new.txt" actual
	)
'

test_expect_success 'diff-index --cached --exit-code exits 1 with staged deletion' '
	(
	cd repo &&
	test_must_fail grit diff-index --cached --exit-code HEAD
	)
'

# ---- multiple changes ----

test_expect_success 'setup: multiple simultaneous changes' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "delete new.txt" &&
	echo "mod" >>file2.txt &&
	echo "mod" >>file3.txt &&
	grit add file2.txt
	)
'

test_expect_success 'diff-index HEAD shows both staged and unstaged' '
	(
	cd repo &&
	grit diff-index HEAD >actual &&
	grep "file2.txt" actual &&
	grep "file3.txt" actual
	)
'

test_expect_success 'diff-index --cached HEAD shows only staged file' '
	(
	cd repo &&
	grit diff-index --cached HEAD >actual &&
	grep "file2.txt" actual &&
	! grep "file3.txt" actual
	)
'

test_expect_success 'diff-index --exit-code exits 1 with mixed changes' '
	(
	cd repo &&
	test_must_fail grit diff-index --exit-code HEAD
	)
'

test_expect_success 'diff-index --cached --exit-code exits 1 with staged subset' '
	(
	cd repo &&
	test_must_fail grit diff-index --cached --exit-code HEAD
	)
'

# ---- raw output format ----

test_expect_success 'diff-index raw output has colon prefix' '
	(
	cd repo &&
	grit diff-index HEAD >actual &&
	grep "^:" actual
	)
'

test_expect_success 'diff-index raw output has 40-char hashes' '
	(
	cd repo &&
	grit diff-index HEAD >actual &&
	grep -E "[0-9a-f]{40}" actual
	)
'

test_expect_success 'diff-index raw output has status letter' '
	(
	cd repo &&
	grit diff-index HEAD >actual &&
	grep -E "[ADMR]	" actual
	)
'

test_done
