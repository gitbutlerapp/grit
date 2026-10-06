#!/bin/sh

test_description='log --follow rename tracking'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: create file, rename it, modify it' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "hello" >greeting.txt &&
	git add greeting.txt &&
	git commit -m "add greeting" &&
	git mv greeting.txt hello.txt &&
	git commit -m "rename to hello" &&
	echo "world" >>hello.txt &&
	git add hello.txt &&
	git commit -m "modify hello"
	)
'

test_expect_success 'log without --follow shows only post-rename commits' '
	(
	cd repo &&
	git log --oneline -- hello.txt >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'log --follow shows pre-rename history too' '
	(
	cd repo &&
	git log --follow --oneline -- hello.txt >actual &&
	test $(wc -l <actual) -eq 3 &&
	grep "add greeting" actual
	)
'

test_expect_success 'log --follow -n1 limits output' '
	(
	cd repo &&
	git log --follow -n1 --oneline -- hello.txt >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'log --follow on unrenamed file works normally' '
	(
	cd repo &&
	echo "extra" >extra.txt &&
	git add extra.txt &&
	git commit -m "add extra" &&
	git log --follow --oneline -- extra.txt >actual &&
	test $(wc -l <actual) -eq 1 &&
	grep "add extra" actual
	)
'

test_done
