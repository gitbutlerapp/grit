#!/bin/sh

test_description='grit rebase --exec runs command after each commit'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=main
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	git init repo &&
	(
	cd repo &&
	echo base >file.txt &&
	git add file.txt &&
	git commit -m "base" &&

	git checkout -b topic &&
	echo A >a.txt && git add a.txt && git commit -m "add A" &&
	echo B >b.txt && git add b.txt && git commit -m "add B" &&
	echo C >c.txt && git add c.txt && git commit -m "add C"
	)
'

test_expect_success 'rebase --exec runs command after each commit' '
	(
	cd repo &&
	git checkout topic &&
	git rebase --exec "echo EXEC_RAN >>../exec_log" main &&
	test_line_count = 3 ../exec_log
	)
'

test_expect_success 'rebase --exec with failing command aborts' '
	(
	cd repo &&
	git checkout -b fail-topic main &&
	echo D >d.txt && git add d.txt && git commit -m "add D" &&
	echo E >e.txt && git add e.txt && git commit -m "add E" &&

	git checkout fail-topic &&
	test_must_fail git rebase --exec "false" main
	)
'

test_done
