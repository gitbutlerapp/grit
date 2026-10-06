#!/bin/sh
# Tests for grit status output formats: --porcelain, -s, -b, -z
# Note: grit --porcelain always includes ## branch header (unlike git)

test_description='grit status output formats'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup base repo' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo "initial" >file.txt &&
	git add file.txt &&
	git commit -m "initial"
	)
'

# === --porcelain ===

test_expect_success 'porcelain on clean repo is empty' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'porcelain shows added file with A prefix' '
	(
	cd repo &&
	echo "new" >new.txt &&
	git add new.txt &&
	git status --porcelain >../actual &&
	grep "^A  new.txt" ../actual
	)
'

test_expect_success 'porcelain shows modified staged file with M prefix' '
	(
	cd repo &&
	echo "changed" >file.txt &&
	git add file.txt &&
	git status --porcelain >../actual &&
	grep "^M  file.txt" ../actual
	)
'

test_expect_success 'porcelain commit and cleanup for next tests' '
	(
	cd repo &&
	git commit -m "staged stuff" &&
	rm -f new.txt &&
	git rm -f new.txt 2>/dev/null;
	echo "initial" >file.txt &&
	git add file.txt &&
	git commit -m "restore" 2>/dev/null;
	true
	)
'

test_expect_success 'porcelain shows untracked file with ?? prefix' '
	(
	cd repo &&
	echo "untracked" >loose.txt &&
	git status --porcelain >../actual &&
	grep "^?? loose.txt" ../actual &&
	rm -f loose.txt
	)
'

test_expect_success 'porcelain shows deleted file with _D prefix' '
	(
	cd repo &&
	rm file.txt &&
	git status --porcelain >../actual &&
	grep "^ D file.txt" ../actual &&
	git checkout -- file.txt
	)
'

test_expect_success 'porcelain shows staged deletion with D prefix' '
	(
	cd repo &&
	git rm -f file.txt &&
	git status --porcelain >../actual &&
	grep "^D  file.txt" ../actual &&
	git reset HEAD file.txt &&
	git checkout -- file.txt
	)
'

test_expect_success 'porcelain shows both staged and unstaged modification (MM)' '
	(
	cd repo &&
	echo "staged" >file.txt &&
	git add file.txt &&
	echo "worktree" >file.txt &&
	git status --porcelain >../actual &&
	grep "^MM file.txt" ../actual &&
	git checkout -- file.txt &&
	git reset HEAD file.txt 2>/dev/null;
	git checkout -- file.txt 2>/dev/null;
	true
	)
'

# === -s / --short ===

test_expect_success 'short output on clean repo is empty' '
	(
	cd repo &&
	git status -s >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'short shows untracked with ??' '
	(
	cd repo &&
	echo "x" >untrack.txt &&
	git status -s >../actual &&
	grep "^?? untrack.txt" ../actual &&
	rm untrack.txt
	)
'

test_expect_success 'short shows staged add with A' '
	(
	cd repo &&
	echo "added" >added.txt &&
	git add added.txt &&
	git status -s >../actual &&
	grep "^A  added.txt" ../actual &&
	git rm -f added.txt 2>/dev/null;
	git reset HEAD 2>/dev/null;
	true
	)
'

test_expect_success 'short shows worktree modification with _M' '
	(
	cd repo &&
	echo "mod" >file.txt &&
	git status -s >../actual &&
	grep "^ M file.txt" ../actual &&
	git checkout -- file.txt
	)
'

# === -b / --branch ===

test_expect_success 'branch flag shows branch in short output' '
	(
	cd repo &&
	git status -s -b >../actual &&
	head -1 ../actual | grep "##.*main"
	)
'

test_expect_success 'branch flag with porcelain shows branch header' '
	(
	cd repo &&
	git status --porcelain -b >../actual &&
	head -1 ../actual | grep "##"
	)
'

test_expect_success 'branch flag on detached HEAD' '
	(
	cd repo &&
	git checkout --detach HEAD &&
	git status -s -b >../actual &&
	head -1 ../actual | grep "HEAD" &&
	git checkout main
	)
'

# === -z (NUL termination) ===

test_expect_success 'z flag uses NUL terminator instead of newline' '
	(
	cd repo &&
	echo "ztest" >zfile.txt &&
	git add zfile.txt &&
	git status --porcelain -z >../actual_z &&
	tr "\0" "X" <../actual_z >../actual_z_tr &&
	grep "A  zfile.txt" ../actual_z_tr
	)
'

test_expect_success 'z flag with multiple entries' '
	(
	cd repo &&
	echo "z2" >zfile2.txt &&
	git status --porcelain -z >../actual_z &&
	tr "\0" "\n" <../actual_z >../actual_z_nl &&
	grep "A  zfile.txt" ../actual_z_nl &&
	grep "?? zfile2.txt" ../actual_z_nl
	)
'

test_expect_success 'z flag with short format' '
	(
	cd repo &&
	git status -s -z >../actual_z &&
	tr "\0" "\n" <../actual_z >../actual_z_nl &&
	grep "zfile" ../actual_z_nl
	)
'

test_expect_success 'z flag cleanup' '
	(
	cd repo &&
	rm -f zfile2.txt &&
	git reset HEAD zfile.txt &&
	rm -f zfile.txt
	)
'

# === combined flags ===

test_expect_success 'porcelain and branch combined' '
	(
	cd repo &&
	echo "combo" >combo.txt &&
	git add combo.txt &&
	git status --porcelain -b >../actual &&
	head -1 ../actual | grep "##" &&
	grep "A  combo.txt" ../actual
	)
'

test_expect_success 'short and branch and z combined' '
	(
	cd repo &&
	git status -s -b -z >../actual_z &&
	tr "\0" "\n" <../actual_z >../actual_z_nl &&
	grep "##" ../actual_z_nl &&
	grep "combo.txt" ../actual_z_nl
	)
'

test_expect_success 'porcelain output has no color codes' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	if grep -P "\x1b\[" ../actual 2>/dev/null; then
		echo "porcelain output contains color codes"
		return 1
	fi
	)
'

test_expect_success 'porcelain clean after commit is empty' '
	(
	cd repo &&
	git commit -m "add combo" &&
	git status --porcelain >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'short with no changes after commit is empty' '
	(
	cd repo &&
	git status -s >../actual &&
	test_must_be_empty ../actual
	)
'

test_done
