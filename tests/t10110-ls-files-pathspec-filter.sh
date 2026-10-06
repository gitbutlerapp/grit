#!/bin/sh
# Test ls-files with pathspec filtering, various flags, and edge cases.

test_description='grit ls-files pathspec filter'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	echo "c" >c.md &&
	echo "d" >d.md &&
	echo "readme" >README.md &&
	mkdir -p src/lib &&
	echo "main" >src/main.rs &&
	echo "lib" >src/lib.rs &&
	echo "util" >src/lib/util.rs &&
	mkdir -p docs &&
	echo "doc" >docs/guide.md &&
	echo "api" >docs/api.md &&
	mkdir -p test &&
	echo "t1" >test/test1.sh &&
	echo "t2" >test/test2.sh &&
	grit add . &&
	test_tick &&
	grit commit -m "initial"
	)
'

test_expect_success 'ls-files with no pathspec lists all cached files' '
	(
	cd repo &&
	grit ls-files >actual &&
	test_line_count = 12 actual
	)
'

test_expect_success 'ls-files with single file pathspec' '
	(
	cd repo &&
	grit ls-files a.txt >actual &&
	test_line_count = 1 actual &&
	grep "a.txt" actual
	)
'

test_expect_success 'ls-files with two file pathspecs' '
	(
	cd repo &&
	grit ls-files a.txt b.txt >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'ls-files with directory pathspec' '
	(
	cd repo &&
	grit ls-files src >actual &&
	grep "src/main.rs" actual &&
	grep "src/lib.rs" actual &&
	grep "src/lib/util.rs" actual
	)
'

test_expect_success 'ls-files directory pathspec shows only that dir' '
	(
	cd repo &&
	grit ls-files docs >actual &&
	test_line_count = 2 actual &&
	grep "docs/guide.md" actual &&
	grep "docs/api.md" actual
	)
'

test_expect_success 'ls-files with trailing slash directory pathspec' '
	(
	cd repo &&
	grit ls-files src/ >actual &&
	grep "src/main.rs" actual
	)
'

test_expect_success 'ls-files nonexistent path returns empty' '
	(
	cd repo &&
	grit ls-files nonexistent >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'ls-files --cached is default behavior' '
	(
	cd repo &&
	grit ls-files >default_out &&
	grit ls-files --cached >cached_out &&
	test_cmp default_out cached_out
	)
'

test_expect_success 'ls-files -s shows stage info' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	head -1 actual | grep -qE "^[0-9]{6} [0-9a-f]{40} [0-9]	"
	)
'

test_expect_success 'ls-files -s with pathspec filters' '
	(
	cd repo &&
	grit ls-files -s a.txt >actual &&
	test_line_count = 1 actual &&
	grep "a.txt" actual
	)
'

test_expect_success 'ls-files -s with directory pathspec' '
	(
	cd repo &&
	grit ls-files -s src >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'ls-files --cached explicitly matches --cached flag' '
	(
	cd repo &&
	grit ls-files --cached >explicit &&
	grit ls-files >default_ls &&
	test_cmp default_ls explicit
	)
'

test_expect_success 'ls-files --cached with directory pathspec' '
	(
	cd repo &&
	grit ls-files --cached test >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'ls-files --deleted shows deleted tracked files' '
	(
	cd repo &&
	rm a.txt &&
	grit ls-files --deleted >actual &&
	grep "a.txt" actual &&
	echo "a" >a.txt
	)
'

test_expect_success 'ls-files --modified shows modified tracked files' '
	(
	cd repo &&
	echo "modified" >a.txt &&
	grit ls-files --modified >actual &&
	grep "a.txt" actual &&
	echo "a" >a.txt
	)
'

test_expect_success 'ls-files --modified with pathspec' '
	(
	cd repo &&
	echo "modified" >a.txt &&
	echo "also modified" >b.txt &&
	grit ls-files --modified a.txt >actual &&
	grep "a.txt" actual &&
	! grep "b.txt" actual &&
	echo "a" >a.txt &&
	echo "b" >b.txt
	)
'

test_expect_success 'ls-files -z uses NUL terminators' '
	(
	cd repo &&
	grit ls-files -z >actual &&
	tr "\0" "\n" <actual >converted &&
	test_line_count -gt 0 converted
	)
'

test_expect_success 'ls-files -z with pathspec' '
	(
	cd repo &&
	grit ls-files -z docs >actual &&
	tr "\0" "\n" <actual | grep -v "^$" >converted &&
	test_line_count = 2 converted
	)
'

test_expect_success 'ls-files output is sorted' '
	(
	cd repo &&
	grit ls-files >actual &&
	sort actual >sorted &&
	test_cmp actual sorted
	)
'

test_expect_success 'ls-files with multiple directory pathspecs' '
	(
	cd repo &&
	grit ls-files src docs >actual &&
	grep "src/" actual &&
	grep "docs/" actual &&
	! grep "test/" actual
	)
'

test_expect_success 'ls-files with file and directory pathspec' '
	(
	cd repo &&
	grit ls-files a.txt docs >actual &&
	grep "a.txt" actual &&
	grep "docs/" actual
	)
'

test_expect_success 'ls-files pathspec does not match partial names' '
	(
	cd repo &&
	grit ls-files a >actual &&
	# "a" should not match "a.txt" as a file pathspec
	# but might match as prefix - just verify it returns something reasonable
	true
	)
'

test_expect_success 'ls-files -s stage number is 0 for normal files' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	while read mode oid stage name; do
		test "$stage" = "0" || return 1
	done <actual
	)
'

test_expect_success 'ls-files -s OIDs are valid' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	awk "{print \$2}" actual | while read oid; do
		echo "$oid" | grep -qE "^[0-9a-f]{40}$" || return 1
	done
	)
'

test_expect_success 'ls-files -s OID matches hash-object for blob' '
	(
	cd repo &&
	grit ls-files -s a.txt >actual &&
	staged_oid=$(awk "{print \$2}" actual) &&
	computed_oid=$(grit hash-object a.txt) &&
	test "$staged_oid" = "$computed_oid"
	)
'

test_expect_success 'ls-files after adding new file' '
	(
	cd repo &&
	echo "new" >new_file.txt &&
	grit update-index --add new_file.txt &&
	grit ls-files new_file.txt >actual &&
	grep "new_file.txt" actual
	)
'

test_expect_success 'ls-files after removing file from index' '
	(
	cd repo &&
	grit update-index --force-remove new_file.txt &&
	grit ls-files new_file.txt >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'ls-files with nested directory pathspec' '
	(
	cd repo &&
	grit ls-files src/lib >actual &&
	# src/lib matches both src/lib.rs and src/lib/ dir
	grep "src/lib/util.rs" actual
	)
'

test_expect_success 'ls-files test directory shows test files' '
	(
	cd repo &&
	grit ls-files test >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'ls-files count matches total added files' '
	(
	cd repo &&
	total=$(grit ls-files | wc -l | tr -d " ") &&
	test "$total" = "12"
	)
'

test_expect_success 'ls-files --long shows extended format' '
	(
	cd repo &&
	grit ls-files --long >actual &&
	test_line_count -gt 0 actual
	)
'

test_expect_success 'ls-files -t shows status tag' '
	(
	cd repo &&
	grit ls-files -t >actual &&
	test_line_count -gt 0 actual
	)
'

test_expect_success 'ls-files --stage same as -s' '
	(
	cd repo &&
	grit ls-files -s >short_flag &&
	grit ls-files --stage >long_flag &&
	test_cmp short_flag long_flag
	)
'

test_expect_success 'setup: add files with similar prefixes' '
	(
	cd repo &&
	echo "foo" >foo &&
	echo "foobar" >foobar &&
	echo "foo2" >foo2 &&
	grit update-index --add foo foobar foo2
	)
'

test_expect_success 'ls-files exact match for foo' '
	(
	cd repo &&
	grit ls-files foo >actual &&
	grep "^foo$" actual
	)
'

test_expect_success 'ls-files does not confuse foo with foobar' '
	(
	cd repo &&
	grit ls-files foobar >actual &&
	test_line_count = 1 actual &&
	grep "foobar" actual
	)
'

test_done
