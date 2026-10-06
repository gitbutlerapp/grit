#!/bin/sh

test_description='grit instaweb embedded server'

. ./test-lib.sh

test_expect_success 'grit-git release artifact includes grit-instaweb helper' '
	helper="$(dirname "$GUST_BIN")/grit-instaweb" &&
	test -x "$helper"
'

test_expect_success 'instaweb start serves home page' '
	port=$((20000 + ($$ % 15000))) &&
	grit init instaweb-repo &&
	(
		cd instaweb-repo &&
		test_write_lines hello >world.txt &&
		grit add world.txt &&
		grit commit -m "first" &&
		grit instaweb --start --port=$port
	) &&
	test -f instaweb-repo/.git/instaweb/pid &&
	curl -sf "http://127.0.0.1:$port/" >page &&
	grep -F "Grit instaweb" page &&
	grep -F "instaweb-repo" page &&
	grep -F "first" page &&
	(
		cd instaweb-repo &&
		grit instaweb --stop
	) &&
	test ! -f instaweb-repo/.git/instaweb/pid &&
	! curl -sf "http://127.0.0.1:$port/" >/dev/null 2>&1
'

test_expect_success 'instaweb stop clears pid file' '
	port=$((20000 + ($$ % 15000) + 1)) &&
	grit init instaweb-stop &&
	(
		cd instaweb-stop &&
		grit instaweb --start --port=$port &&
		grit instaweb --stop
	) &&
	test ! -f instaweb-stop/.git/instaweb/pid &&
	! curl -sf "http://127.0.0.1:$port/" >/dev/null 2>&1
'

test_done
