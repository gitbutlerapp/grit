#!/bin/sh

test_description='grit diff --inter-hunk-context=N

Tests that --inter-hunk-context is accepted. This flag merges
nearby hunks when the gap between them is <= N lines.
For now we verify the flag is accepted without error.'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "test@test.com" &&
	for i in $(seq 1 40); do echo "line$i"; done >file.txt &&
	git add file.txt &&
	git commit -m "initial" &&
	sed "s/^line5$/MOD5/" file.txt | sed "s/^line10$/MOD10/" >tmp &&
	mv tmp file.txt &&
	git add file.txt &&
	git commit -m "modify lines 5 and 10"
	)
'

test_expect_success 'diff --inter-hunk-context=0 is accepted' '
	(
	cd repo &&
	git diff --inter-hunk-context=0 HEAD~1 HEAD >out &&
	test -s out
	)
'

test_expect_success 'diff --inter-hunk-context=10 is accepted' '
	(
	cd repo &&
	git diff --inter-hunk-context=10 HEAD~1 HEAD >out &&
	test -s out
	)
'

test_expect_success 'diff -U0 --inter-hunk-context=0 produces output' '
	(
	cd repo &&
	git diff -U0 --inter-hunk-context=0 HEAD~1 HEAD >out &&
	test -s out
	)
'

test_done
