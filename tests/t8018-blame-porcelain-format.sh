#!/bin/sh

test_description='blame --porcelain format correctness'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	git init blame-porc &&
	cd blame-porc &&
	git config user.name "A U Thor" &&
	git config user.email "author@example.com" &&
	echo "line 1" >file &&
	echo "line 2" >>file &&
	echo "line 3" >>file &&
	git add file &&
	test_tick &&
	git commit -m "initial" &&

	echo "line 1 mod" >file &&
	echo "line 2" >>file &&
	echo "line 3 mod" >>file &&
	echo "line 4" >>file &&
	echo "line 5" >>file &&
	git add file &&
	test_tick &&
	git commit -m "modify"
	)
'

test_expect_success 'porcelain has boundary for root commit' '
	(
	cd blame-porc &&
	git blame --porcelain file >actual &&
	grep "^boundary$" actual
	)
'

test_expect_success 'porcelain has previous for non-root commit' '
	(
	cd blame-porc &&
	git blame --porcelain file >actual &&
	grep "^previous " actual
	)
'

test_expect_success 'porcelain group counts are correct' '
	(
	cd blame-porc &&
	git blame --porcelain file >actual &&
	# line 3-5 are from same commit, group count should be 3 on first line
	HEAD_SHA=$(git rev-parse HEAD) &&
	# Find the line with group count 3
	grep "^$HEAD_SHA .* 3$" actual
	)
'

test_expect_success 'porcelain continuation lines omit group count' '
	(
	cd blame-porc &&
	git blame --porcelain file >actual &&
	HEAD_SHA=$(git rev-parse HEAD) &&
	# Lines 4 and 5 should not have a trailing count
	grep "^$HEAD_SHA 4 4$" actual &&
	grep "^$HEAD_SHA 5 5$" actual
	)
'

test_expect_success 'porcelain summary uses first non-blank line' '
	(
	cd blame-porc &&
	TREE=$(git write-tree) &&
	commit=$(printf "%s\n%s\n%s\n\n\n  \nactual subject\n\nbody\n" \
		"tree $TREE" \
		"author A <a@b.c> 123456789 +0000" \
		"committer C <c@d.e> 123456789 +0000" |
	git hash-object -w -t commit --stdin) &&
	git blame --porcelain $commit -- file >output &&
	grep "^summary actual subject" output
	)
'

test_done
