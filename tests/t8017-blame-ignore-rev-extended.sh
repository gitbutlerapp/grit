#!/bin/sh

test_description='blame --ignore-rev and --ignore-revs-file'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	git init blame-ignore-ext &&
	cd blame-ignore-ext &&
	git config user.name "Test" &&
	git config user.email "test@test.com" &&
	echo "line 1" >file &&
	echo "line 2" >>file &&
	git add file &&
	test_tick &&
	git commit -m "initial" &&
	git tag INITIAL &&

	echo "LINE 1" >file &&
	echo "LINE 2" >>file &&
	git add file &&
	test_tick &&
	git commit -m "reformatting" &&
	git tag REFORMAT
	)
'

test_expect_success 'blame --ignore-rev skips commit' '
	(
	cd blame-ignore-ext &&
	REFORMAT_SHA=$(git rev-parse REFORMAT) &&
	git blame --ignore-rev "$REFORMAT_SHA" --line-porcelain file >actual &&
	INITIAL_SHA=$(git rev-parse INITIAL) &&
	grep "^$INITIAL_SHA" actual
	)
'

test_expect_success 'blame --ignore-revs-file skips commits' '
	(
	cd blame-ignore-ext &&
	git rev-parse REFORMAT >ignore-file &&
	git blame --ignore-revs-file ignore-file --line-porcelain file >actual &&
	INITIAL_SHA=$(git rev-parse INITIAL) &&
	grep "^$INITIAL_SHA" actual
	)
'

test_expect_success 'ignore-revs-file supports comments and blank lines' '
	(
	cd blame-ignore-ext &&
	{
		echo "# this is a comment" &&
		echo "" &&
		git rev-parse REFORMAT &&
		echo ""
	} >ignore-file2 &&
	git blame --ignore-revs-file ignore-file2 --line-porcelain file >actual &&
	INITIAL_SHA=$(git rev-parse INITIAL) &&
	grep "^$INITIAL_SHA" actual
	)
'

test_done
