#!/bin/sh
test_description='git grep extended features: -E, -P, -f, -e, --and, --all-match, --threads'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	cat >file1 <<-\EOF &&
	foo bar baz
	hello world
	foo hello
	something else
	EOF
	cat >file2 <<-\EOF &&
	alpha beta
	foo gamma
	delta epsilon
	EOF
	git add . &&
	git commit -m "initial"
	)
'

test_expect_success 'grep -E extended regexp alternation' '
	(
	cd repo &&
	git grep -E "foo|alpha" >actual &&
	test $(wc -l <actual) -eq 4
	)
'

test_expect_success 'grep -P perl regexp accepted' '
	(
	cd repo &&
	git grep -P "foo" >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'grep -e pattern (explicit)' '
	(
	cd repo &&
	git grep -e "hello" >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'grep -e pattern -e pattern (multiple)' '
	(
	cd repo &&
	git grep -e "hello" -e "alpha" >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'grep -f reads patterns from file' '
	(
	cd repo &&
	cat >patterns.txt <<-\EOF &&
	hello
	alpha
	EOF
	git grep -f patterns.txt >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'grep --all-match requires all patterns' '
	(
	cd repo &&
	git grep --all-match -e "foo" -e "hello" >actual &&
	test $(wc -l <actual) -eq 1 &&
	grep "foo hello" actual
	)
'

test_expect_success 'grep --threads is accepted' '
	(
	cd repo &&
	git grep --threads=4 "foo" >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'grep pattern tree-ish -- pathspec' '
	(
	cd repo &&
	git grep "foo" HEAD -- file1 >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'grep -E with grouping' '
	(
	cd repo &&
	git grep -E "(hello|alpha)" >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'grep --and with -e patterns' '
	(
	cd repo &&
	git grep --and -e "foo" -e "bar" >actual &&
	test $(wc -l <actual) -eq 1 &&
	grep "foo bar baz" actual
	)
'

test_done
