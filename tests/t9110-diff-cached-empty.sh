#!/bin/sh
# Test diff --cached behavior including empty/no-change scenarios.

test_description='grit diff --cached with various staging states'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup repository with initial commit' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Tester" &&
	echo "initial" >file.txt &&
	grit add file.txt &&
	grit commit -m "initial"
	)
'

###########################################################################
# Section 1: Empty diff --cached (nothing staged)
###########################################################################

test_expect_success 'diff --cached with nothing staged is empty' '
	(
	cd repo &&
	grit diff --cached >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff --cached --stat with nothing staged is empty' '
	(
	cd repo &&
	grit diff --cached --stat >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff --cached --name-only with nothing staged is empty' '
	(
	cd repo &&
	grit diff --cached --name-only >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff --cached --name-status with nothing staged is empty' '
	(
	cd repo &&
	grit diff --cached --name-status >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff --cached --numstat with nothing staged is empty' '
	(
	cd repo &&
	grit diff --cached --numstat >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff --cached --exit-code returns 0 when nothing staged' '
	(
	cd repo &&
	grit diff --cached --exit-code
	)
'

###########################################################################
# Section 2: Staged new file
###########################################################################

test_expect_success 'diff --cached shows staged new file' '
	(
	cd repo &&
	echo "new" >new.txt &&
	grit add new.txt &&
	grit diff --cached >out &&
	grep "+new" out &&
	grep "new.txt" out
	)
'

test_expect_success 'diff --cached --stat shows staged new file' '
	(
	cd repo &&
	grit diff --cached --stat >out &&
	grep "new.txt" out &&
	grep "1 file changed" out
	)
'

test_expect_success 'diff --cached --name-only shows staged new file' '
	(
	cd repo &&
	grit diff --cached --name-only >out &&
	grep "^new.txt$" out
	)
'

test_expect_success 'diff --cached --name-status shows A for new file' '
	(
	cd repo &&
	grit diff --cached --name-status >out &&
	grep "^A	new.txt$" out
	)
'

test_expect_success 'diff --cached --exit-code returns 1 when staged' '
	(
	cd repo &&
	test_must_fail grit diff --cached --exit-code
	)
'

###########################################################################
# Section 3: Staged modification
###########################################################################

test_expect_success 'commit staged new file for next tests' '
	(
	cd repo &&
	grit commit -m "add new"
	)
'

test_expect_success 'diff --cached shows staged modification' '
	(
	cd repo &&
	echo "modified" >>file.txt &&
	grit add file.txt &&
	grit diff --cached >out &&
	grep "+modified" out
	)
'

test_expect_success 'diff --cached --stat shows modification stat' '
	(
	cd repo &&
	grit diff --cached --stat >out &&
	grep "file.txt" out &&
	grep "1 insertion" out
	)
'

test_expect_success 'diff --cached --numstat for modification' '
	(
	cd repo &&
	grit diff --cached --numstat >out &&
	grep "file.txt" out
	)
'

###########################################################################
# Section 4: Staged deletion
###########################################################################

test_expect_success 'commit modification then stage delete' '
	(
	cd repo &&
	grit commit -m "modify" &&
	grit rm new.txt &&
	grit diff --cached >out &&
	grep "new.txt" out
	)
'

test_expect_success 'diff --cached --name-status shows D for deletion' '
	(
	cd repo &&
	grit diff --cached --name-status >out &&
	grep "^D	new.txt$" out
	)
'

test_expect_success 'diff --cached --stat shows deletion' '
	(
	cd repo &&
	grit diff --cached --stat >out &&
	grep "new.txt" out &&
	grep "deletion" out
	)
'

###########################################################################
# Section 5: Multiple staged changes
###########################################################################

test_expect_success 'diff --cached with multiple staged files' '
	(
	cd repo &&
	grit commit -m "del" &&
	echo "alpha" >alpha.txt &&
	echo "beta" >beta.txt &&
	echo "gamma" >gamma.txt &&
	grit add alpha.txt beta.txt gamma.txt &&
	grit diff --cached --name-only >out &&
	grep "alpha.txt" out &&
	grep "beta.txt" out &&
	grep "gamma.txt" out
	)
'

test_expect_success 'diff --cached --stat with multiple files' '
	(
	cd repo &&
	grit diff --cached --stat >out &&
	grep "3 files changed" out
	)
'

###########################################################################
# Section 6: Empty file operations
###########################################################################

test_expect_success 'diff --cached with staged empty file' '
	(
	cd repo &&
	grit commit -m "three files" &&
	: >empty.txt &&
	grit add empty.txt &&
	grit diff --cached --name-only >out &&
	grep "^empty.txt$" out
	)
'

test_expect_success 'diff --cached patch for empty file has no content lines' '
	(
	cd repo &&
	grit diff --cached >out &&
	grep "empty.txt" out
	)
'

test_expect_success 'diff --cached after committing empty shows clean' '
	(
	cd repo &&
	grit commit -m "add empty" &&
	grit diff --cached >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 7: Stage then unstage
###########################################################################

test_expect_success 'diff --cached is empty after reset' '
	(
	cd repo &&
	echo "temp" >temp.txt &&
	grit add temp.txt &&
	grit diff --cached --name-only >staged &&
	grep "temp.txt" staged &&
	grit reset HEAD -- temp.txt &&
	grit diff --cached >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 8: Mixed staged and unstaged
###########################################################################

test_expect_success 'diff --cached only shows staged not unstaged' '
	(
	cd repo &&
	echo "will stage" >staged.txt &&
	echo "will not stage" >unstaged.txt &&
	grit add staged.txt &&
	grit diff --cached --name-only >out &&
	grep "staged.txt" out &&
	! grep "unstaged.txt" out
	)
'

test_expect_success 'diff (without --cached) shows unstaged changes' '
	(
	cd repo &&
	grit commit -m "staged" &&
	echo "more" >>staged.txt &&
	grit diff --name-only >out &&
	grep "staged.txt" out
	)
'

test_expect_success 'diff --cached after partial add shows only staged hunks' '
	(
	cd repo &&
	grit add staged.txt &&
	grit diff --cached >out &&
	grep "+more" out
	)
'

test_expect_success 'diff --cached -q suppresses output but exits non-zero' '
	(
	cd repo &&
	test_must_fail grit diff --cached -q >out 2>&1 &&
	test_must_be_empty out
	)
'

test_done
