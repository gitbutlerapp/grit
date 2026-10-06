#!/bin/sh

test_description='blame -L extended range formats'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	git init blame-lrange &&
	cd blame-lrange &&
	git config user.name "Test" &&
	git config user.email "test@test.com" &&
	test_write_lines "line 1" "line 2" "line 3" "line 4" "line 5" >file &&
	git add file &&
	git commit -m "initial" &&
	test_write_lines "line 1 mod" "line 2" "line 3 mod" "line 4" "line 5" >file &&
	git add file &&
	git commit -m "modify"
	)
'

test_expect_success 'blame -L N,+M shows M lines' '
	(
	cd blame-lrange &&
	git blame -L 2,+2 file >actual &&
	test_line_count = 2 actual &&
	grep "line 2" actual &&
	grep "line 3" actual
	)
'

test_expect_success 'blame -L /regex/,/regex/ works' '
	(
	cd blame-lrange &&
	git blame -L "/line 3/,/line 5/" file >actual &&
	test_line_count = 3 actual &&
	grep "line 3" actual &&
	grep "line 5" actual
	)
'

test_expect_success 'blame -L /regex/,+N works' '
	(
	cd blame-lrange &&
	git blame -L "/line 2/,+2" file >actual &&
	test_line_count = 2 actual &&
	grep "line 2" actual &&
	grep "line 3" actual
	)
'

test_expect_success 'blame -L N,$ shows to end' '
	(
	cd blame-lrange &&
	git blame -L "3,$" file >actual &&
	test_line_count = 3 actual &&
	grep "line 3" actual &&
	grep "line 5" actual
	)
'

test_done
