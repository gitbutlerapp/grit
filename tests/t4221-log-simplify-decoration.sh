#!/bin/sh
test_description='grit log --simplify-by-decoration

Tests --simplify-by-decoration: only show commits that are decorated.'

. ./test-lib.sh

test_expect_success 'setup: repo with branches and tags' '
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
	git tag v1.0 &&
	test_tick &&
	echo d >file.txt && git add file.txt && git commit -m "fourth" &&
	test_tick &&
	echo e >file.txt && git add file.txt && git commit -m "fifth"
	)
'

test_expect_success '--simplify-by-decoration shows only decorated commits' '
	(
	cd repo &&
	grit log --simplify-by-decoration --oneline >out &&
	grep "fifth" out &&
	grep "third" out &&
	! grep "second" out &&
	! grep "first" out
	)
'

test_expect_success '--simplify-by-decoration shows HEAD and tagged commits' '
	(
	cd repo &&
	grit log --simplify-by-decoration --format="tformat:%s" >out &&
	# HEAD (fifth) and v1.0 (third) should appear
	test $(wc -l <out) -eq 2
	)
'

test_expect_success '--simplify-by-decoration with --all' '
	(
	cd repo &&
	grit log --simplify-by-decoration --all --oneline >out &&
	# Should still only show decorated commits
	grep "fifth" out &&
	! grep "fourth" out
	)
'

test_done
