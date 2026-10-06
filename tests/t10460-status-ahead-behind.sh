#!/bin/sh

test_description='status --ahead-behind tracking branch display'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: create repo with upstream' '
	(
	git init upstream &&
	cd upstream &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo a >file.txt &&
	git add file.txt &&
	git commit -m "initial" &&
	cd .. &&
	git clone upstream local &&
	cd local &&
	git config user.name "Test" &&
	git config user.email "test@example.com"
	)
'

test_expect_success 'status -sb shows tracking with no divergence' '
	(
	cd local &&
	git status -sb >actual &&
	head -1 actual >branch_line &&
	grep "main\.\.\.origin/main" branch_line
	)
'

test_expect_success 'status -sb shows ahead count after local commit' '
	(
	cd local &&
	echo b >>file.txt &&
	git add file.txt &&
	git commit -m "local change" &&
	git status -sb >actual &&
	head -1 actual >branch_line &&
	grep "ahead 1" branch_line
	)
'

test_expect_success 'status -sb shows behind count after upstream advances' '
	(
	cd upstream &&
	echo c >>file.txt &&
	git add file.txt &&
	git commit -m "upstream change" &&
	cd ../local &&
	git fetch &&
	git status -sb >actual &&
	head -1 actual >branch_line &&
	grep "behind 1" branch_line
	)
'

test_expect_success 'status --no-ahead-behind suppresses count' '
	(
	cd local &&
	git status -sb --no-ahead-behind >actual &&
	head -1 actual >branch_line &&
	! grep "ahead" branch_line &&
	! grep "behind" branch_line
	)
'

test_done
