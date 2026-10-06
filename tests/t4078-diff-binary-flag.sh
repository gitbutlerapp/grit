#!/bin/sh

test_description='grit diff --binary

Tests that --binary produces GIT binary patch output instead of
"Binary files differ" message.'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup binary repo' '
	(
	$REAL_GIT init binrepo &&
	cd binrepo &&
	$REAL_GIT config user.name "Test" &&
	$REAL_GIT config user.email "test@test.com" &&
	printf "\000\001\002\003initial-binary" >bin.dat &&
	echo "text content" >text.txt &&
	$REAL_GIT add . &&
	$REAL_GIT commit -m "initial"
	)
'

test_expect_success 'diff --binary shows GIT binary patch' '
	(
	cd binrepo &&
	printf "\000\001\002\004changed-binary" >bin.dat &&
	$REAL_GIT add bin.dat &&
	$REAL_GIT commit -m "modify binary" &&
	grit diff --binary HEAD~1 HEAD >out &&
	grep "GIT binary patch" out
	)
'

test_expect_success 'diff --binary shows literal line' '
	(
	cd binrepo &&
	grit diff --binary HEAD~1 HEAD >out &&
	grep "^literal " out
	)
'

test_expect_success 'diff without --binary shows Binary files differ' '
	(
	cd binrepo &&
	grit diff HEAD~1 HEAD >out &&
	grep "Binary files.*differ" out
	)
'

test_expect_success 'diff --binary for added binary file' '
	(
	cd binrepo &&
	printf "\000\001\002" >new-bin.dat &&
	$REAL_GIT add new-bin.dat &&
	$REAL_GIT commit -m "add new binary" &&
	grit diff --binary HEAD~1 HEAD >out &&
	grep "GIT binary patch" out
	)
'

test_done
