#!/bin/sh

test_description='git -v identifies the grit build'

. ./test-lib.sh

test_expect_success 'git -v output contains grit' '
	git -v >out 2>&1 &&
	grep -q grit out
'

test_done
