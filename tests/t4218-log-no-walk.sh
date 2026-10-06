#!/bin/sh
test_description='grit log --no-walk

Tests --no-walk: show given commits without walking parents.'

. ./test-lib.sh

test_expect_success 'setup: repo with several commits' '
	(
	git init repo &&
	cd repo &&
	git config user.name "A U Thor" &&
	git config user.email "author@example.com" &&
	test_tick &&
	echo a >file.txt && git add file.txt && git commit -m "first" &&
	test_tick &&
	echo b >file.txt && git add file.txt && git commit -m "second" &&
	test_tick &&
	echo c >file.txt && git add file.txt && git commit -m "third" &&
	test_tick &&
	echo d >file.txt && git add file.txt && git commit -m "fourth"
	)
'

test_expect_success '--no-walk HEAD shows only HEAD commit' '
	(
	cd repo &&
	grit log --no-walk --oneline HEAD >out &&
	test $(wc -l <out) -eq 1 &&
	grep "fourth" out
	)
'

test_expect_success '--no-walk with multiple revisions shows only those' '
	(
	cd repo &&
	grit log --no-walk --oneline HEAD HEAD~2 >out &&
	test $(wc -l <out) -eq 2 &&
	grep "fourth" out &&
	grep "second" out
	)
'

test_expect_success '--no-walk does not include parents' '
	(
	cd repo &&
	grit log --no-walk --oneline HEAD~1 >out &&
	test $(wc -l <out) -eq 1 &&
	grep "third" out &&
	! grep "second" out &&
	! grep "first" out
	)
'

test_expect_success '--no-walk with --reverse' '
	(
	cd repo &&
	grit log --no-walk --reverse --oneline HEAD HEAD~3 >out &&
	test $(wc -l <out) -eq 2 &&
	head -1 out | grep "first" &&
	tail -1 out | grep "fourth"
	)
'

test_expect_success '--no-walk default (no args) shows HEAD' '
	(
	cd repo &&
	grit log --no-walk --oneline >out &&
	test $(wc -l <out) -eq 1 &&
	grep "fourth" out
	)
'

test_expect_success '--no-walk with --format' '
	(
	cd repo &&
	grit log --no-walk --format="tformat:%s" HEAD HEAD~1 >out &&
	test $(wc -l <out) -eq 2 &&
	grep "fourth" out &&
	grep "third" out
	)
'

test_done
