#!/bin/sh
test_description='grit log --source

Tests --source: show which ref led to each commit.'

. ./test-lib.sh

test_expect_success 'setup: repo with branches' '
	(
	git init repo &&
	cd repo &&
	git config user.name "A U Thor" &&
	git config user.email "author@example.com" &&
	test_tick &&
	echo a >file.txt && git add file.txt && git commit -m "initial" &&
	test_tick &&
	echo b >file.txt && git add file.txt && git commit -m "second" &&
	git checkout -b feature &&
	test_tick &&
	echo c >file.txt && git add file.txt && git commit -m "feature-commit" &&
	git checkout main
	)
'

test_expect_success '--source with --all shows ref names' '
	(
	cd repo &&
	grit log --all --source --oneline >out &&
	grep "feature" out
	)
'

test_expect_success '--source output contains tab separator' '
	(
	cd repo &&
	grit log --all --source --oneline >out &&
	grep "	" out
	)
'

test_expect_success '--source shows each commit with a ref' '
	(
	cd repo &&
	grit log --all --source --oneline >out &&
	# Every line should have a tab (source\thash subject)
	while read line; do
		echo "$line" | grep "	" || exit 1
	done <out
	)
'

test_done
