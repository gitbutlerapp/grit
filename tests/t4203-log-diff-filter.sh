#!/bin/sh

test_description='log --diff-filter and --all'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo a >f1.txt &&
	git add f1.txt &&
	git commit -m "add f1" &&
	echo b >f2.txt &&
	git add f2.txt &&
	git commit -m "add f2" &&
	echo c >>f1.txt &&
	git add f1.txt &&
	git commit -m "modify f1" &&
	git rm f2.txt &&
	git commit -m "delete f2"
	)
'

test_expect_success 'log --diff-filter=A shows only commits with additions' '
	(
	cd repo &&
	git log --diff-filter=A --oneline >actual &&
	test $(wc -l <actual) -eq 2 &&
	grep "add f1" actual &&
	grep "add f2" actual
	)
'

test_expect_success 'log --diff-filter=M shows only commits with modifications' '
	(
	cd repo &&
	git log --diff-filter=M --oneline >actual &&
	test $(wc -l <actual) -eq 1 &&
	grep "modify f1" actual
	)
'

test_expect_success 'log --diff-filter=D shows only commits with deletions' '
	(
	cd repo &&
	git log --diff-filter=D --oneline >actual &&
	test $(wc -l <actual) -eq 1 &&
	grep "delete f2" actual
	)
'

test_expect_success 'log --diff-filter=AM shows additions and modifications' '
	(
	cd repo &&
	git log --diff-filter=AM --oneline >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'log --all shows commits from all branches' '
	(
	cd repo &&
	git branch feature &&
	git checkout feature &&
	echo feat >feat.txt &&
	git add feat.txt &&
	git commit -m "feature commit" &&
	git checkout main &&
	git log --all --oneline >actual &&
	grep "feature commit" actual &&
	grep "delete f2" actual
	)
'

test_expect_success 'log --all --oneline shows all commits' '
	(
	cd repo &&
	git log --all --oneline >actual &&
	test $(wc -l <actual) -eq 5
	)
'

test_done
