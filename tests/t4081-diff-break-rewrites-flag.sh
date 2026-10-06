#!/bin/sh

test_description='grit diff -B / --break-rewrites

Tests that -B flag is accepted. The flag is intended to
break complete rewrites into delete+add pairs.'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.name "Test" &&
	$REAL_GIT config user.email "test@test.com" &&
	printf "line1\nline2\nline3\nline4\nline5\n" >file.txt &&
	$REAL_GIT add file.txt &&
	$REAL_GIT commit -m "initial" &&
	printf "alpha\nbeta\ngamma\ndelta\nepsilon\n" >file.txt &&
	$REAL_GIT add file.txt &&
	$REAL_GIT commit -m "rewrite"
	)
'

test_expect_success 'diff -B is accepted' '
	(
	cd repo &&
	grit diff -B HEAD~1 HEAD >out &&
	test -s out
	)
'

test_expect_success 'diff --break-rewrites is accepted' '
	(
	cd repo &&
	grit diff --break-rewrites HEAD~1 HEAD >out &&
	test -s out
	)
'

test_expect_success 'diff -B shows file changes' '
	(
	cd repo &&
	grit diff -B --name-status HEAD~1 HEAD >out &&
	grep "file.txt" out
	)
'

test_done
