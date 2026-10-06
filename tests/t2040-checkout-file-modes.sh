#!/bin/sh
# Tests for checkout preserving/changing file permissions.

test_description='grit checkout file mode handling'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ===========================================================================
# Setup
# ===========================================================================

test_expect_success 'setup: init repo with regular and executable files' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "normal file" >normal.txt &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	git add normal.txt script.sh &&
	git commit -m "initial with normal and executable files"
	)
'

# ===========================================================================
# Index records correct modes
# ===========================================================================

test_expect_success 'ls-files shows 100644 for normal file' '
	(
	cd repo &&
	git ls-files --stage normal.txt >actual &&
	grep "^100644" actual
	)
'

test_expect_success 'ls-files shows 100755 for executable file' '
	(
	cd repo &&
	git ls-files --stage script.sh >actual &&
	grep "^100755" actual
	)
'

# ===========================================================================
# cat-file on tree confirms modes
# ===========================================================================

test_expect_success 'tree entry shows 100644 for normal file' '
	(
	cd repo &&
	tree_oid=$(git rev-parse HEAD^{tree}) &&
	git cat-file -p "$tree_oid" >actual &&
	grep "100644 blob.*normal.txt" actual
	)
'

test_expect_success 'tree entry shows 100755 for executable file' '
	(
	cd repo &&
	tree_oid=$(git rev-parse HEAD^{tree}) &&
	git cat-file -p "$tree_oid" >actual &&
	grep "100755 blob.*script.sh" actual
	)
'

# ===========================================================================
# Checkout preserves modes
# ===========================================================================

test_expect_success 'checkout branch preserves executable bit' '
	(
	cd repo &&
	git branch test-branch &&
	git checkout test-branch &&
	test -x script.sh &&
	git checkout master
	)
'

test_expect_success 'checkout to new branch preserves file modes' '
	(
	cd repo &&
	git checkout -b mode-test &&
	test -x script.sh &&
	test ! -x normal.txt &&
	git checkout master
	)
'

test_expect_success 'checkout restores executable bit after deletion' '
	(
	cd repo &&
	rm script.sh &&
	git checkout -- script.sh &&
	test -x script.sh
	)
'

test_expect_success 'checkout restores normal mode after deletion' '
	(
	cd repo &&
	rm normal.txt &&
	git checkout -- normal.txt &&
	test -f normal.txt &&
	test ! -x normal.txt
	)
'

# ===========================================================================
# Changing modes via chmod + git add
# ===========================================================================

test_expect_success 'chmod +x then git add changes mode in index' '
	(
	cd repo &&
	git checkout -b chmod-test &&
	chmod +x normal.txt &&
	git add normal.txt &&
	git ls-files --stage normal.txt >actual &&
	grep "^100755" actual
	)
'

test_expect_success 'committing mode change preserves it in tree' '
	(
	cd repo &&
	git commit -m "make normal.txt executable" &&
	tree_oid=$(git rev-parse HEAD^{tree}) &&
	git cat-file -p "$tree_oid" >actual &&
	grep "100755 blob.*normal.txt" actual
	)
'

test_expect_success 'chmod -x then git add removes executable bit in index' '
	(
	cd repo &&
	chmod -x normal.txt &&
	git add normal.txt &&
	git ls-files --stage normal.txt >actual &&
	grep "^100644" actual &&
	git commit -m "revert normal.txt to non-executable"
	)
'

test_expect_success 'checkout between branches with different modes' '
	(
	cd repo &&
	git checkout master &&
	git ls-files --stage normal.txt >actual &&
	grep "^100644" actual &&
	git checkout chmod-test &&
	git ls-files --stage normal.txt >actual &&
	grep "^100644" actual
	)
'

# ===========================================================================
# diff detects mode changes
# ===========================================================================

test_expect_success 'diff detects mode change from 644 to 755' '
	(
	cd repo &&
	git checkout master &&
	chmod +x normal.txt &&
	git add normal.txt &&
	git diff --cached --name-only >actual &&
	grep "normal.txt" actual
	)
'

test_expect_success 'diff --cached shows old/new mode lines' '
	(
	cd repo &&
	git diff --cached >actual &&
	grep "old mode 100644" actual &&
	grep "new mode 100755" actual &&
	chmod -x normal.txt &&
	git add normal.txt &&
	git reset HEAD normal.txt
	)
'

# ===========================================================================
# checkout-index preserves modes
# ===========================================================================

test_expect_success 'checkout-index restores executable file with correct mode' '
	(
	cd repo &&
	rm -f script.sh &&
	git checkout-index -f script.sh &&
	test -x script.sh
	)
'

test_expect_success 'checkout-index restores normal file with correct mode' '
	(
	cd repo &&
	rm -f normal.txt &&
	git checkout-index -f normal.txt &&
	test -f normal.txt
	)
'

# ===========================================================================
# Multiple files with mixed modes
# ===========================================================================

test_expect_success 'setup: add more files with mixed modes' '
	(
	cd repo &&
	git checkout master &&
	echo "#!/usr/bin/env python" >run.py &&
	chmod +x run.py &&
	echo "data" >data.csv &&
	echo "#!/bin/bash" >build.sh &&
	chmod +x build.sh &&
	git add run.py data.csv build.sh &&
	git commit -m "add mixed mode files"
	)
'

test_expect_success 'ls-files --stage shows correct modes for all files' '
	(
	cd repo &&
	git ls-files --stage >actual &&
	grep "^100755.*build.sh" actual &&
	grep "^100644.*data.csv" actual &&
	grep "^100644.*normal.txt" actual &&
	grep "^100755.*run.py" actual &&
	grep "^100755.*script.sh" actual
	)
'

test_expect_success 'checkout-index -a restores all files with correct modes' '
	(
	cd repo &&
	rm -f build.sh data.csv run.py script.sh normal.txt &&
	git checkout-index -a -f &&
	test -x build.sh &&
	test ! -x data.csv &&
	test ! -x normal.txt &&
	test -x run.py &&
	test -x script.sh
	)
'

test_expect_success 'tree entries all have correct modes after checkout' '
	(
	cd repo &&
	tree_oid=$(git rev-parse HEAD^{tree}) &&
	git cat-file -p "$tree_oid" >actual &&
	grep "100755 blob.*build.sh" actual &&
	grep "100644 blob.*data.csv" actual &&
	grep "100755 blob.*run.py" actual &&
	grep "100755 blob.*script.sh" actual
	)
'

# ===========================================================================
# Switching branches with mode differences
# ===========================================================================

test_expect_success 'setup: branch with different modes via chmod+add' '
	(
	cd repo &&
	git checkout -b mode-branch &&
	chmod -x build.sh &&
	chmod +x data.csv &&
	git add build.sh data.csv &&
	git commit -m "swap modes on build.sh and data.csv"
	)
'

test_expect_success 'checkout master restores original modes' '
	(
	cd repo &&
	git checkout master &&
	test -x build.sh &&
	test ! -x data.csv
	)
'

test_expect_success 'checkout mode-branch applies changed modes' '
	(
	cd repo &&
	git checkout mode-branch &&
	git ls-files --stage build.sh >actual &&
	grep "^100644" actual &&
	git ls-files --stage data.csv >actual &&
	grep "^100755" actual
	)
'

test_expect_success 'switching back to master restores modes again' '
	(
	cd repo &&
	git checkout master &&
	git ls-files --stage build.sh >actual &&
	grep "^100755" actual &&
	git ls-files --stage data.csv >actual &&
	grep "^100644" actual
	)
'

test_expect_success 'working tree modes match index after branch switch' '
	(
	cd repo &&
	test -x build.sh &&
	test ! -x data.csv &&
	git checkout mode-branch &&
	test ! -x build.sh &&
	test -x data.csv
	)
'

# ===========================================================================
# Edge cases
# ===========================================================================

test_expect_success 'chmod +x on new file then add records 100755' '
	(
	cd repo &&
	git checkout master &&
	echo "#!/bin/sh" >newexec.sh &&
	chmod +x newexec.sh &&
	git add newexec.sh &&
	git ls-files --stage newexec.sh >actual &&
	grep "^100755" actual
	)
'

test_expect_success 'restore --staged then re-add loses exec bit' '
	(
	cd repo &&
	git restore --staged newexec.sh &&
	chmod -x newexec.sh &&
	git add newexec.sh &&
	git ls-files --stage newexec.sh >actual &&
	grep "^100644" actual
	)
'

test_done
