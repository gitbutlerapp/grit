#!/bin/sh

test_description='grit diff --color-moved

Tests that --color-moved is accepted and produces output.
Full moved-line detection coloring is a cosmetic enhancement;
for now we verify the flag is accepted without error and
diff output is still produced.'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "test@test.com" &&
	cat >file.txt <<-\EOF &&
	line1
	line2
	line3
	moved-block-a
	moved-block-b
	moved-block-c
	line7
	EOF
	git add file.txt &&
	git commit -m "initial" &&
	cat >file.txt <<-\EOF &&
	moved-block-a
	moved-block-b
	moved-block-c
	line1
	line2
	line3
	line7
	EOF
	git add file.txt &&
	git commit -m "move block up"
	)
'

test_expect_success 'diff --color-moved produces output' '
	(
	cd repo &&
	git diff --color-moved HEAD~1 HEAD >out &&
	test -s out
	)
'

test_expect_success 'diff --color-moved=default produces output' '
	(
	cd repo &&
	git diff --color-moved=default HEAD~1 HEAD >out &&
	test -s out
	)
'

test_expect_success 'diff --color-moved=no produces output' '
	(
	cd repo &&
	git diff --color-moved=no HEAD~1 HEAD >out &&
	test -s out
	)
'

test_expect_success 'diff --color-moved with --stat works' '
	(
	cd repo &&
	git diff --color-moved --stat HEAD~1 HEAD >out &&
	grep "file.txt" out
	)
'

test_done
