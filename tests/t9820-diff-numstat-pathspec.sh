#!/bin/sh
# Tests for grit diff --numstat with various pathspec patterns.

test_description='grit diff --numstat with pathspecs'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repository with multiple directories and files' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	mkdir -p src/lib src/bin docs tests/unit tests/integration &&
	echo "main code" >src/lib/core.c &&
	echo "header" >src/lib/core.h &&
	echo "binary entry" >src/bin/main.c &&
	echo "readme" >docs/README.md &&
	echo "guide" >docs/GUIDE.txt &&
	echo "unit1" >tests/unit/test1.c &&
	echo "integ1" >tests/integration/test1.c &&
	echo "root file" >Makefile &&
	echo "another" >config.yml &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial commit"
	)
'

test_expect_success 'setup: make changes across all directories' '
	(
	cd repo &&
	echo "main code v2" >src/lib/core.c &&
	echo "header v2" >src/lib/core.h &&
	echo "binary entry v2" >src/bin/main.c &&
	echo "readme v2" >docs/README.md &&
	echo "guide v2" >docs/GUIDE.txt &&
	echo "unit1 v2" >tests/unit/test1.c &&
	echo "integ1 v2" >tests/integration/test1.c &&
	echo "root file v2" >Makefile &&
	echo "another v2" >config.yml &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "update all files"
	)
'

###########################################################################
# Section 2: Basic --numstat without pathspec
###########################################################################

test_expect_success 'diff --numstat shows all changed files' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --numstat counts lines correctly' '
	(
	cd repo &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD >actual &&
	test $(wc -l <actual) -eq 9
	)
'

###########################################################################
# Section 3: Single file pathspec
###########################################################################

test_expect_success 'diff --numstat with single file pathspec' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD -- Makefile >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- Makefile >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --numstat single file shows one line' '
	(
	cd repo &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- Makefile >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'diff --numstat with nonexistent pathspec shows nothing' '
	(
	cd repo &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- nonexistent.txt >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 4: Directory pathspec
###########################################################################

test_expect_success 'diff --numstat with directory pathspec' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD -- src/ >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- src/ >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --numstat with nested directory pathspec' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD -- src/lib/ >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- src/lib/ >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --numstat src/lib/ shows only lib files' '
	(
	cd repo &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- src/lib/ >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'diff --numstat tests/ includes both unit and integration' '
	(
	cd repo &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- tests/ >actual &&
	test $(wc -l <actual) -eq 2
	)
'

###########################################################################
# Section 5: Glob pathspecs
###########################################################################

test_expect_success 'diff --numstat with *.c glob' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD -- "*.c" >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- "*.c" >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --numstat with *.md glob' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD -- "*.md" >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- "*.md" >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --numstat with *.h glob' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD -- "*.h" >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- "*.h" >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --numstat *.h matches git output count' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD -- "*.h" >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- "*.h" >actual &&
	test $(wc -l <actual) -eq $(wc -l <expected)
	)
'

###########################################################################
# Section 6: Multiple pathspecs
###########################################################################

test_expect_success 'diff --numstat with two file pathspecs' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD -- Makefile config.yml >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- Makefile config.yml >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --numstat with two directory pathspecs' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD -- src/ docs/ >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- src/ docs/ >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --numstat with mixed file and directory pathspecs' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD -- Makefile src/lib/ >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- Makefile src/lib/ >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 7: Pathspec with subdirectory cwd
###########################################################################

test_expect_success 'diff --numstat from subdirectory' '
	(
	cd repo/src &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD -- lib/ >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- lib/ >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --numstat from subdirectory with ..' '
	(
	cd repo/src &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD -- ../docs/ >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- ../docs/ >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 8: Staged diff --numstat
###########################################################################

test_expect_success 'setup: stage changes for cached diff' '
	(
	cd repo &&
	echo "v3 core" >src/lib/core.c &&
	echo "v3 makefile" >Makefile &&
	"$REAL_GIT" add src/lib/core.c Makefile
	)
'

test_expect_success 'diff --cached --numstat without pathspec' '
	(
	cd repo &&
	"$REAL_GIT" diff --cached --numstat >expected &&
	"$GUST_BIN" diff --cached --numstat >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --cached --numstat shows correct file count' '
	(
	cd repo &&
	"$REAL_GIT" diff --cached --numstat >expected &&
	"$GUST_BIN" diff --cached --numstat >actual &&
	test $(wc -l <actual) -eq $(wc -l <expected)
	)
'

test_expect_success 'diff --cached --numstat shows only staged files' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --numstat >actual &&
	test $(wc -l <actual) -eq 2
	)
'

###########################################################################
# Section 9: Working tree diff --numstat
###########################################################################

test_expect_success 'setup: make unstaged changes' '
	(
	cd repo &&
	"$REAL_GIT" commit -m "v3 partial" &&
	echo "unstaged docs" >docs/README.md &&
	echo "unstaged config" >config.yml
	)
'

test_expect_success 'diff --numstat (working tree) without pathspec' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat >expected &&
	"$GUST_BIN" diff --numstat >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --numstat (working tree) shows correct count' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat >expected &&
	"$GUST_BIN" diff --numstat >actual &&
	test $(wc -l <actual) -eq $(wc -l <expected)
	)
'

test_expect_success 'diff --numstat (working tree) lines contain filenames' '
	(
	cd repo &&
	"$GUST_BIN" diff --numstat >actual &&
	grep "README.md" actual &&
	grep "config.yml" actual
	)
'

###########################################################################
# Section 10: Multi-line changes and numstat values
###########################################################################

test_expect_success 'setup: create file with multiple lines' '
	(
	cd repo &&
	"$REAL_GIT" checkout -b multiline &&
	for i in $(seq 1 20); do echo "line $i"; done >bigfile.txt &&
	"$REAL_GIT" add bigfile.txt &&
	"$REAL_GIT" commit -m "add bigfile"
	)
'

test_expect_success 'setup: modify multiple lines' '
	(
	cd repo &&
	for i in $(seq 1 20); do
		if test $((i % 3)) -eq 0; then
			echo "modified line $i"
		else
			echo "line $i"
		fi
	done >bigfile.txt &&
	"$REAL_GIT" add bigfile.txt &&
	"$REAL_GIT" commit -m "modify some lines"
	)
'

test_expect_success 'diff --numstat shows correct add/delete counts' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD -- bigfile.txt >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- bigfile.txt >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --numstat values are tab-separated' '
	(
	cd repo &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- bigfile.txt >actual &&
	grep "	" actual
	)
'

###########################################################################
# Section 11: Pathspec with renames
###########################################################################

test_expect_success 'setup: rename a file' '
	(
	cd repo &&
	"$REAL_GIT" mv src/lib/core.h src/lib/header.h &&
	"$REAL_GIT" commit -m "rename core.h to header.h"
	)
'

test_expect_success 'diff --numstat with pathspec after rename' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD -- src/lib/ >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- src/lib/ >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'diff --numstat with *.h after rename' '
	(
	cd repo &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD -- "*.h" >expected &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- "*.h" >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 12: Empty diffs
###########################################################################

test_expect_success 'diff --numstat with no changes is empty' '
	(
	cd repo &&
	"$GUST_BIN" diff --numstat HEAD HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff --numstat with pathspec matching unchanged files is empty' '
	(
	cd repo &&
	"$GUST_BIN" diff --numstat HEAD~1 HEAD -- Makefile >actual &&
	test_must_be_empty actual
	)
'

test_done
