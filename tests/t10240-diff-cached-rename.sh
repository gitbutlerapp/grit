#!/bin/sh
# Tests for diff-index --cached with renames, additions, deletions,
# mode changes, and multi-path scenarios.

test_description='grit diff-index --cached rename and staging tests'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup initial commit with several files' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@example.com" &&
	git config user.name "Test User" &&
	echo "alpha" >a.txt &&
	echo "bravo" >b.txt &&
	echo "charlie" >c.txt &&
	echo "delta" >d.txt &&
	echo "echo" >e.txt &&
	mkdir -p sub &&
	echo "sub-file" >sub/f.txt &&
	echo "sub-other" >sub/g.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "initial"
	)
'

test_expect_success 'diff-index --cached with no changes is empty' '
	(
	cd repo &&
	grit diff-index --cached HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff-index --cached detects staged modification' '
	(
	cd repo &&
	echo "alpha-modified" >a.txt &&
	grit add a.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "M	a.txt" actual
	)
'

test_expect_success 'diff-index --cached exit code 1 when changes exist' '
	(
	cd repo &&
	! grit diff-index --cached --exit-code HEAD >actual
	)
'

test_expect_success 'diff-index --cached --quiet returns 1 for changes' '
	(
	cd repo &&
	! grit diff-index --cached --quiet HEAD
	)
'

test_expect_success 'diff-index --cached shows add as A status' '
	(
	cd repo &&
	echo "new-file" >new.txt &&
	grit add new.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "A	new.txt" actual
	)
'

test_expect_success 'diff-index --cached shows delete as D status' '
	(
	cd repo &&
	grit update-index --force-remove b.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "D	b.txt" actual
	)
'

test_expect_success 'diff-index --cached shows rename as D+A pair' '
	(
	cd repo &&
	grit update-index --force-remove c.txt &&
	cp c.txt renamed-c.txt 2>/dev/null || echo "charlie" >renamed-c.txt &&
	grit add renamed-c.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "D	c.txt" actual &&
	grep "A	renamed-c.txt" actual
	)
'

test_expect_success 'reset index and verify clean state' '
	(
	cd repo &&
	grit read-tree HEAD &&
	grit diff-index --cached HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'staging multiple modifications shows all' '
	(
	cd repo &&
	echo "mod-a" >a.txt &&
	echo "mod-d" >d.txt &&
	echo "mod-e" >e.txt &&
	grit add a.txt d.txt e.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "M	a.txt" actual &&
	grep "M	d.txt" actual &&
	grep "M	e.txt" actual
	)
'

test_expect_success 'diff-index --cached output has correct format' '
	(
	cd repo &&
	grit diff-index --cached HEAD >actual &&
	# Each line should be :old_mode new_mode old_oid new_oid status\tpath
	while IFS= read -r line; do
		echo "$line" | grep -qE "^:[0-9]{6} [0-9]{6} [0-9a-f]{40} [0-9a-f]{40} [AMDTUX]	" || return 1
	done <actual
	)
'

test_expect_success 'reset and stage file in subdirectory' '
	(
	cd repo &&
	grit read-tree HEAD &&
	echo "modified-sub" >sub/f.txt &&
	grit add sub/f.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "M	sub/f.txt" actual
	)
'

test_expect_success 'diff-index --cached with path filter' '
	(
	cd repo &&
	echo "mod-a2" >a.txt &&
	grit add a.txt &&
	grit diff-index --cached HEAD -- a.txt >actual &&
	grep "a.txt" actual &&
	! grep "sub/f.txt" actual
	)
'

test_expect_success 'diff-index --cached with directory path filter' '
	(
	cd repo &&
	grit diff-index --cached HEAD -- sub >actual &&
	grep "sub/f.txt" actual &&
	! grep "a.txt" actual
	)
'

test_expect_success 'staging new file in subdirectory shows A' '
	(
	cd repo &&
	echo "new-sub" >sub/new.txt &&
	grit add sub/new.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "A	sub/new.txt" actual
	)
'

test_expect_success 'removing file from subdirectory shows D' '
	(
	cd repo &&
	grit update-index --force-remove sub/g.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "D	sub/g.txt" actual
	)
'

test_expect_success 'reset and test clean state again' '
	(
	cd repo &&
	grit read-tree HEAD &&
	grit diff-index --cached HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'add and remove same file shows no change' '
	(
	cd repo &&
	echo "temp" >temp.txt &&
	grit update-index --add temp.txt &&
	grit update-index --force-remove temp.txt &&
	grit diff-index --cached HEAD >actual &&
	! grep "temp.txt" actual
	)
'

test_expect_success 'staging identical content produces no diff' '
	(
	cd repo &&
	grit read-tree HEAD &&
	grit checkout-index -a -f &&
	grit add a.txt &&
	grit diff-index --cached HEAD >actual &&
	! grep "a.txt" actual
	)
'

test_expect_success 'diff-index --cached exit 0 when no changes' '
	(
	cd repo &&
	grit read-tree HEAD &&
	grit diff-index --cached --exit-code HEAD >actual &&
	test $? -eq 0
	)
'

test_expect_success 'diff-index --cached --quiet exit 0 when no changes' '
	(
	cd repo &&
	grit diff-index --cached --quiet HEAD
	)
'

test_expect_success 'stage deletion of all files' '
	(
	cd repo &&
	grit ls-files >all_files &&
	while read f; do
		grit update-index --force-remove "$f"
	done <all_files &&
	grit diff-index --cached HEAD >actual &&
	count=$(wc -l <actual | tr -d " ") &&
	test "$count" -ge 5
	)
'

test_expect_success 'all deleted files show D status' '
	(
	cd repo &&
	grit diff-index --cached HEAD >actual &&
	while IFS= read -r line; do
		echo "$line" | grep -q "	D	\|D	" || return 1
	done <actual
	)
'

test_expect_success 'reset and stage only one file keeps others clean' '
	(
	cd repo &&
	grit read-tree HEAD &&
	echo "only-a" >a.txt &&
	grit add a.txt &&
	grit diff-index --cached HEAD >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'diff-index --cached OIDs are valid hex' '
	(
	cd repo &&
	grit diff-index --cached HEAD >actual &&
	while IFS= read -r line; do
		old_oid=$(echo "$line" | awk "{print \$3}") &&
		new_oid=$(echo "$line" | awk "{print \$4}") &&
		echo "$old_oid" | grep -qE "^[0-9a-f]{40}$" &&
		echo "$new_oid" | grep -qE "^[0-9a-f]{40}$" || return 1
	done <actual
	)
'

test_expect_success 'new OID in diff-index matches hash-object' '
	(
	cd repo &&
	grit diff-index --cached HEAD >actual &&
	new_oid=$(awk "{print \$4}" actual | head -1) &&
	computed=$(grit hash-object a.txt) &&
	test "$new_oid" = "$computed"
	)
'

test_expect_success 'reset for mode test setup' '
	(
	cd repo &&
	grit read-tree HEAD &&
	grit checkout-index -a -f
	)
'

test_expect_success 'diff-index --cached with empty tree (all adds)' '
	(
	cd repo &&
	empty_tree=$(printf "" | grit hash-object -w -t tree --stdin) &&
	grit diff-index --cached "$empty_tree" >actual &&
	count=$(wc -l <actual | tr -d " ") &&
	test "$count" -ge 5
	)
'

test_expect_success 'all entries against empty tree are A status' '
	(
	cd repo &&
	empty_tree=$(printf "" | grit hash-object -w -t tree --stdin) &&
	grit diff-index --cached "$empty_tree" >actual &&
	while IFS= read -r line; do
		echo "$line" | grep -q "A	" || return 1
	done <actual
	)
'

test_expect_success 'stage file with special characters in content' '
	(
	cd repo &&
	grit read-tree HEAD &&
	printf "line1\tline2\nline3" >a.txt &&
	grit add a.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "M	a.txt" actual
	)
'

test_expect_success 'multiple sequential add/modify detected' '
	(
	cd repo &&
	grit read-tree HEAD &&
	echo "first" >a.txt &&
	grit add a.txt &&
	echo "second" >a.txt &&
	grit add a.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "M	a.txt" actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'diff-index --cached on brand new repo with root commit' '
	(
	grit init fresh &&
	cd fresh &&
	git config user.email "test@example.com" &&
	git config user.name "Test User" &&
	echo hello >hello.txt &&
	grit add hello.txt &&
	test_tick &&
	grit commit -m "first" &&
	grit diff-index --cached HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff-index --cached compares trees not working dir' '
	(
	cd repo &&
	grit read-tree HEAD &&
	grit checkout-index -a -f &&
	echo "worktree-only" >a.txt &&
	grit diff-index --cached HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff-index without --cached detects worktree changes' '
	(
	cd repo &&
	grit read-tree HEAD &&
	grit checkout-index -a -f &&
	echo "worktree-mod" >a.txt &&
	grit diff-index HEAD >actual &&
	grep "M	a.txt" actual
	)
'

test_done
