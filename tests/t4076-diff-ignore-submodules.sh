#!/bin/sh

test_description='grit diff --ignore-submodules

Tests that --ignore-submodules filters out submodule entries from diff output.'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup repo with submodule' '
	(
	$REAL_GIT init sub &&
	cd sub &&
	$REAL_GIT config user.name "Test" &&
	$REAL_GIT config user.email "test@test.com" &&
	echo "sub content" >sub-file.txt &&
	$REAL_GIT add sub-file.txt &&
	$REAL_GIT commit -m "sub initial" &&
	SUB_COMMIT=$($REAL_GIT rev-parse HEAD) &&
	cd .. &&
	$REAL_GIT init super &&
	cd super &&
	$REAL_GIT config user.name "Test" &&
	$REAL_GIT config user.email "test@test.com" &&
	echo "super content" >file.txt &&
	$REAL_GIT add file.txt &&
	$REAL_GIT commit -m "initial" &&
	$REAL_GIT config protocol.file.allow always &&
	$REAL_GIT -c protocol.file.allow=always submodule add ../sub sub &&
	$REAL_GIT commit -m "add submodule" &&
	echo "modified" >file.txt &&
	$REAL_GIT add file.txt &&
	cd sub &&
	echo "sub modified" >sub-file.txt &&
	$REAL_GIT add sub-file.txt &&
	$REAL_GIT commit -m "sub modify" &&
	cd .. &&
	$REAL_GIT add sub &&
	$REAL_GIT commit -m "update sub and file"
	)
'

test_expect_success 'diff without --ignore-submodules shows submodule' '
	(
	cd super &&
	grit diff --name-only HEAD~1 HEAD >out &&
	grep "file.txt" out &&
	grep "sub" out
	)
'

test_expect_success 'diff --ignore-submodules hides submodule changes' '
	(
	cd super &&
	grit diff --ignore-submodules --name-only HEAD~1 HEAD >out &&
	grep "file.txt" out &&
	! grep "^sub$" out
	)
'

test_expect_success 'diff --ignore-submodules --name-status hides submodule' '
	(
	cd super &&
	grit diff --ignore-submodules --name-status HEAD~1 HEAD >out &&
	grep "file.txt" out &&
	! grep "sub" out
	)
'

test_done
