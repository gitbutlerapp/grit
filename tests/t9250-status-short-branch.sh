#!/bin/sh
# Test status --short and --branch output: branch header, file status
# indicators, staged/unstaged/untracked combinations.

test_description='grit status --short --branch output'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup repository on master' '
	(
	grit init --initial-branch=master repo &&
	cd repo &&
	$REAL_GIT config user.email "test@test.com" &&
	$REAL_GIT config user.name "Tester" &&
	echo "initial" >tracked.txt &&
	echo "other" >other.txt &&
	grit add tracked.txt other.txt &&
	grit commit -m "initial"
	)
'

###########################################################################
# Section 1: Branch header
###########################################################################

test_expect_success 'status -sb shows branch header with ##' '
	(
	cd repo &&
	grit status -sb >../out &&
	grep "^## master" ../out
	)
'

test_expect_success 'status --short --branch shows same header' '
	(
	cd repo &&
	grit status --short --branch >../out &&
	grep "^## master" ../out
	)
'

test_expect_success 'status -s without -b omits branch header' '
	(
	cd repo &&
	grit status -s >../out &&
	! grep "^##" ../out
	)
'

test_expect_success 'status -sb on clean tree shows only branch line' '
	(
	cd repo &&
	grit status -sb >../out &&
	test_line_count = 1 ../out
	)
'

###########################################################################
# Section 2: Untracked files
###########################################################################

test_expect_success 'status -sb shows untracked file with ??' '
	(
	cd repo &&
	echo "new" >untracked.txt &&
	grit status -sb >../out &&
	grep "^?? untracked.txt$" ../out
	)
'

test_expect_success 'status -s shows untracked file with ??' '
	(
	cd repo &&
	grit status -s >../out &&
	grep "^?? untracked.txt$" ../out
	)
'

test_expect_success 'multiple untracked files listed' '
	(
	cd repo &&
	echo "a" >un_a.txt &&
	echo "b" >un_b.txt &&
	grit status -sb >../out &&
	grep "^?? un_a.txt$" ../out &&
	grep "^?? un_b.txt$" ../out
	)
'

test_expect_success 'cleanup untracked files' '
	(
	cd repo &&
	rm -f untracked.txt un_a.txt un_b.txt
	)
'

###########################################################################
# Section 3: Staged additions
###########################################################################

test_expect_success 'status -sb shows staged addition with A' '
	(
	cd repo &&
	echo "added" >added.txt &&
	grit add added.txt &&
	grit status -sb >../out &&
	grep "^A  added.txt$" ../out
	)
'

test_expect_success 'status -s shows staged addition' '
	(
	cd repo &&
	grit status -s >../out &&
	grep "^A  added.txt$" ../out
	)
'

test_expect_success 'multiple staged additions' '
	(
	cd repo &&
	echo "x" >x.txt &&
	echo "y" >y.txt &&
	grit add x.txt y.txt &&
	grit status -sb >../out &&
	grep "^A  x.txt$" ../out &&
	grep "^A  y.txt$" ../out
	)
'

test_expect_success 'commit staged additions and verify clean' '
	(
	cd repo &&
	grit commit -m "add files" &&
	grit status -sb >../out &&
	test_line_count = 1 ../out &&
	grep "^## master" ../out
	)
'

###########################################################################
# Section 4: Staged modifications
###########################################################################

test_expect_success 'status -sb shows staged modification with M in first column' '
	(
	cd repo &&
	echo "modified" >tracked.txt &&
	grit add tracked.txt &&
	grit status -sb >../out &&
	grep "^M  tracked.txt$" ../out
	)
'

test_expect_success 'commit staged modification' '
	(
	cd repo &&
	grit commit -m "modify tracked" &&
	grit status -sb >../out &&
	test_line_count = 1 ../out
	)
'

###########################################################################
# Section 5: Unstaged modifications
###########################################################################

test_expect_success 'status -sb shows unstaged modification with M in second column' '
	(
	cd repo &&
	echo "unstaged change" >tracked.txt &&
	grit status -sb >../out &&
	grep "^ M tracked.txt$" ../out
	)
'

test_expect_success 'status -s shows unstaged modification' '
	(
	cd repo &&
	grit status -s >../out &&
	grep "^ M tracked.txt$" ../out
	)
'

test_expect_success 'staging unstaged modification moves to first column' '
	(
	cd repo &&
	grit add tracked.txt &&
	grit status -sb >../out &&
	grep "^M  tracked.txt$" ../out
	)
'

test_expect_success 'commit and clean' '
	(
	cd repo &&
	grit commit -m "update again" &&
	grit status -sb >../out &&
	test_line_count = 1 ../out
	)
'

###########################################################################
# Section 6: Staged deletion
###########################################################################

test_expect_success 'status -sb shows staged deletion with D' '
	(
	cd repo &&
	grit rm x.txt &&
	grit status -sb >../out &&
	grep "^D  x.txt$" ../out
	)
'

test_expect_success 'commit deletion and verify clean' '
	(
	cd repo &&
	grit commit -m "remove x" &&
	grit status -sb >../out &&
	test_line_count = 1 ../out
	)
'

###########################################################################
# Section 7: Mixed staged and unstaged
###########################################################################

test_expect_success 'status -sb shows staged and unstaged for same file' '
	(
	cd repo &&
	echo "stage this" >tracked.txt &&
	grit add tracked.txt &&
	echo "then change more" >tracked.txt &&
	grit status -sb >../out &&
	grep "^MM tracked.txt$" ../out
	)
'

test_expect_success 'status -sb shows different files in different states' '
	(
	cd repo &&
	echo "unstaged" >other.txt &&
	grit status -sb >../out &&
	grep "^MM tracked.txt$" ../out &&
	grep "^ M other.txt$" ../out
	)
'

test_expect_success 'commit -a equivalent: add all and commit' '
	(
	cd repo &&
	grit add tracked.txt other.txt &&
	grit commit -m "mixed changes" &&
	grit status -sb >../out &&
	test_line_count = 1 ../out
	)
'

###########################################################################
# Section 8: Untracked directory
###########################################################################

test_expect_success 'status -sb shows untracked directory' '
	(
	cd repo &&
	mkdir -p newdir &&
	echo "inside" >newdir/file.txt &&
	grit status -sb >../out &&
	grep "newdir" ../out
	)
'

test_expect_success 'adding directory changes to staged' '
	(
	cd repo &&
	grit add newdir/ &&
	grit status -sb >../out &&
	grep "^A  newdir/file.txt$" ../out
	)
'

test_expect_success 'commit directory and verify clean' '
	(
	cd repo &&
	grit commit -m "add newdir" &&
	grit status -sb >../out &&
	test_line_count = 1 ../out
	)
'

###########################################################################
# Section 9: Branch header on different branches
###########################################################################

test_expect_success 'status -sb shows feature branch name' '
	(
	cd repo &&
	$REAL_GIT checkout -b feature &&
	grit status -sb >../out &&
	grep "^## feature" ../out
	)
'

test_expect_success 'status -sb after switching back to master' '
	(
	cd repo &&
	$REAL_GIT checkout master &&
	grit status -sb >../out &&
	grep "^## master" ../out
	)
'

test_expect_success 'status -sb on branch with slash in name' '
	(
	cd repo &&
	$REAL_GIT checkout -b feat/my-thing &&
	grit status -sb >../out &&
	grep "^## feat/my-thing" ../out &&
	$REAL_GIT checkout master
	)
'

###########################################################################
# Section 10: Multiple state combinations
###########################################################################

test_expect_success 'status -sb with all state types at once' '
	(
	cd repo &&
	echo "staged add" >sa.txt &&
	grit add sa.txt &&
	echo "modify" >tracked.txt &&
	echo "untracked" >zzz.txt &&
	grit status -sb >../out &&
	grep "^## master" ../out &&
	grep "^A  sa.txt$" ../out &&
	grep "^ M tracked.txt$" ../out &&
	grep "^?? zzz.txt$" ../out
	)
'

test_expect_success 'status -s without -b has no branch header but same file entries' '
	(
	cd repo &&
	grit status -s >../out &&
	! grep "^##" ../out &&
	grep "^A  sa.txt$" ../out &&
	grep "^ M tracked.txt$" ../out &&
	grep "^?? zzz.txt$" ../out
	)
'

test_expect_success 'cleanup and final verify' '
	(
	cd repo &&
	rm zzz.txt &&
	grit add tracked.txt &&
	grit commit -m "final mixed" &&
	grit status -sb >../out &&
	test_line_count = 1 ../out
	)
'

test_done
