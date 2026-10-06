#!/bin/sh
# Tests for grit status with various staged states

test_description='grit status staged changes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup base repo' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo "line1" >a.txt &&
	echo "line1" >b.txt &&
	echo "line1" >c.txt &&
	mkdir sub &&
	echo "sub" >sub/d.txt &&
	git add . &&
	git commit -m "initial"
	)
'

# === staged add ===

test_expect_success 'status shows staged new file' '
	(
	cd repo &&
	echo "new" >new.txt &&
	git add new.txt &&
	git status --porcelain >../actual &&
	grep "^A  new.txt" ../actual
	)
'

test_expect_success 'status shows multiple staged new files' '
	(
	cd repo &&
	echo "x" >x.txt &&
	echo "y" >y.txt &&
	git add x.txt y.txt &&
	git status --porcelain >../actual &&
	grep "^A  x.txt" ../actual &&
	grep "^A  y.txt" ../actual
	)
'

test_expect_success 'status shows staged file in subdirectory' '
	(
	cd repo &&
	echo "new sub" >sub/e.txt &&
	git add sub/e.txt &&
	git status --porcelain >../actual &&
	grep "^A  sub/e.txt" ../actual
	)
'

test_expect_success 'commit staged adds and verify clean' '
	(
	cd repo &&
	git commit -m "add files" &&
	git status -s >../actual &&
	test_must_be_empty ../actual
	)
'

# === staged modify ===

test_expect_success 'status shows staged modification' '
	(
	cd repo &&
	echo "modified" >a.txt &&
	git add a.txt &&
	git status --porcelain >../actual &&
	grep "^M  a.txt" ../actual
	)
'

test_expect_success 'status shows multiple staged modifications' '
	(
	cd repo &&
	echo "mod b" >b.txt &&
	echo "mod c" >c.txt &&
	git add b.txt c.txt &&
	git status --porcelain >../actual &&
	grep "^M  b.txt" ../actual &&
	grep "^M  c.txt" ../actual
	)
'

test_expect_success 'status staged modify in subdir' '
	(
	cd repo &&
	echo "mod sub" >sub/d.txt &&
	git add sub/d.txt &&
	git status --porcelain >../actual &&
	grep "^M  sub/d.txt" ../actual
	)
'

test_expect_success 'commit staged mods and verify clean' '
	(
	cd repo &&
	git commit -m "modify files" &&
	git status -s >../actual &&
	test_must_be_empty ../actual
	)
'

# === staged rm ===

test_expect_success 'status shows staged deletion' '
	(
	cd repo &&
	git rm x.txt &&
	git status --porcelain >../actual &&
	grep "^D  x.txt" ../actual
	)
'

test_expect_success 'status shows staged deletion of subdir file' '
	(
	cd repo &&
	git rm sub/e.txt &&
	git status --porcelain >../actual &&
	grep "^D  sub/e.txt" ../actual
	)
'

test_expect_success 'multiple staged deletions' '
	(
	cd repo &&
	git rm y.txt &&
	git status --porcelain >../actual &&
	grep "^D  x.txt" ../actual &&
	grep "^D  y.txt" ../actual &&
	grep "^D  sub/e.txt" ../actual
	)
'

test_expect_success 'commit staged deletions' '
	(
	cd repo &&
	git commit -m "delete files" &&
	git status -s >../actual &&
	test_must_be_empty ../actual
	)
'

# === staged rename (add+rm) ===

test_expect_success 'status shows rename as R or delete + add' '
	(
	cd repo &&
	git mv a.txt a-renamed.txt &&
	git status --porcelain >../actual &&
	(grep "^R" ../actual || (grep "^D  a.txt" ../actual && grep "^A  a-renamed.txt" ../actual))
	)
'

test_expect_success 'commit rename' '
	(
	cd repo &&
	git commit -m "rename a" &&
	git status -s >../actual &&
	test_must_be_empty ../actual
	)
'

# === staged mode change ===

test_expect_success 'status shows staged mode change' '
	(
	cd repo &&
	chmod +x b.txt &&
	git add b.txt &&
	git status --porcelain >../actual &&
	grep "b.txt" ../actual
	)
'

test_expect_success 'commit mode change' '
	(
	cd repo &&
	git commit -m "chmod b" &&
	git status -s >../actual &&
	test_must_be_empty ../actual
	)
'

# === mixed staged and unstaged ===

test_expect_success 'status shows staged + unstaged on same file (MM)' '
	(
	cd repo &&
	echo "staged version" >c.txt &&
	git add c.txt &&
	echo "worktree version" >c.txt &&
	git status --porcelain >../actual &&
	grep "^MM c.txt" ../actual
	)
'

test_expect_success 'status -s shows same MM' '
	(
	cd repo &&
	git status -s >../actual &&
	grep "^MM c.txt" ../actual
	)
'

test_expect_success 'staged add with worktree delete (AD)' '
	(
	cd repo &&
	git checkout -- c.txt &&
	git reset HEAD c.txt 2>/dev/null;
	git checkout -- c.txt &&
	echo "temp" >temp.txt &&
	git add temp.txt &&
	rm temp.txt &&
	git status --porcelain >../actual &&
	grep "^AD temp.txt" ../actual
	)
'

test_expect_success 'cleanup mixed state' '
	(
	cd repo &&
	git reset HEAD temp.txt 2>/dev/null;
	rm -f temp.txt;
	git checkout -- . 2>/dev/null;
	true
	)
'

# === multiple types at once ===

test_expect_success 'status shows add, modify, delete simultaneously' '
	(
	cd repo &&
	echo "brand new" >brand.txt &&
	git add brand.txt &&
	echo "changed" >b.txt &&
	git add b.txt &&
	git rm sub/d.txt &&
	git status --porcelain >../actual &&
	grep "^A  brand.txt" ../actual &&
	grep "^M  b.txt" ../actual &&
	grep "^D  sub/d.txt" ../actual
	)
'

test_expect_success 'status short shows same mixed types' '
	(
	cd repo &&
	git status -s >../actual &&
	grep "^A " ../actual &&
	grep "^M " ../actual &&
	grep "^D " ../actual
	)
'

test_expect_success 'commit mixed and verify clean' '
	(
	cd repo &&
	git commit -m "mixed" &&
	git status -s >../actual &&
	test_must_be_empty ../actual
	)
'

# === staging partial changes ===

test_expect_success 'only staged file shows in index column, unstaged in worktree column' '
	(
	cd repo &&
	echo "changed b" >b.txt &&
	echo "changed c" >c.txt &&
	git add b.txt &&
	git status --porcelain >../actual &&
	grep "^M  b.txt" ../actual &&
	grep "^ M c.txt" ../actual
	)
'

test_expect_success 'reset and restore' '
	(
	cd repo &&
	git checkout -- . &&
	git reset HEAD 2>/dev/null;
	git checkout -- . 2>/dev/null;
	true
	)
'

test_done
