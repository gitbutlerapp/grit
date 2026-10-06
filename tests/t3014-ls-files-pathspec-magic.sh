#!/bin/sh
# Tests for ls-files with pathspec patterns: directory prefix, relative paths,
# exact matching, multiple pathspecs, subdirectory context, mixed cases

test_description='ls-files with pathspec patterns'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup repository with nested structure' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "root file" >root.txt &&
	echo "readme" >README.md &&
	mkdir -p src/lib &&
	echo "main" >src/main.c &&
	echo "util" >src/util.c &&
	echo "helper" >src/lib/helper.c &&
	echo "core" >src/lib/core.c &&
	mkdir -p docs &&
	echo "intro" >docs/intro.md &&
	echo "api" >docs/api.md &&
	mkdir -p tests &&
	echo "test1" >tests/test1.sh &&
	echo "test2" >tests/test2.sh &&
	git add . &&
	git commit -m "initial"
	)
'

# ---------------------------------------------------------------------------
# Basic pathspec: exact file
# ---------------------------------------------------------------------------
test_expect_success 'ls-files with exact filename' '
	(
	cd repo &&
	git ls-files root.txt >out &&
	echo "root.txt" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'ls-files with non-existent file returns empty' '
	(
	cd repo &&
	git ls-files nonexistent.txt >out &&
	test_must_be_empty out
	)
'

test_expect_success 'ls-files with multiple exact files' '
	(
	cd repo &&
	git ls-files root.txt README.md >out &&
	cat >expected <<-EOF &&
	README.md
	root.txt
	EOF
	test_cmp expected out
	)
'

# ---------------------------------------------------------------------------
# Directory prefix pathspec
# ---------------------------------------------------------------------------
test_expect_success 'ls-files with directory prefix' '
	(
	cd repo &&
	git ls-files src/ >out &&
	cat >expected <<-EOF &&
	src/lib/core.c
	src/lib/helper.c
	src/main.c
	src/util.c
	EOF
	test_cmp expected out
	)
'

test_expect_success 'ls-files with nested directory prefix' '
	(
	cd repo &&
	git ls-files src/lib/ >out &&
	cat >expected <<-EOF &&
	src/lib/core.c
	src/lib/helper.c
	EOF
	test_cmp expected out
	)
'

test_expect_success 'ls-files with docs directory' '
	(
	cd repo &&
	git ls-files docs/ >out &&
	cat >expected <<-EOF &&
	docs/api.md
	docs/intro.md
	EOF
	test_cmp expected out
	)
'

test_expect_success 'ls-files with tests directory' '
	(
	cd repo &&
	git ls-files tests/ >out &&
	cat >expected <<-EOF &&
	tests/test1.sh
	tests/test2.sh
	EOF
	test_cmp expected out
	)
'

# ---------------------------------------------------------------------------
# Multiple directory pathspecs
# ---------------------------------------------------------------------------
test_expect_success 'ls-files with two directory pathspecs' '
	(
	cd repo &&
	git ls-files docs/ tests/ >out &&
	cat >expected <<-EOF &&
	docs/api.md
	docs/intro.md
	tests/test1.sh
	tests/test2.sh
	EOF
	test_cmp expected out
	)
'

test_expect_success 'ls-files with file and directory pathspec' '
	(
	cd repo &&
	git ls-files root.txt src/lib/ >out &&
	cat >expected <<-EOF &&
	root.txt
	src/lib/core.c
	src/lib/helper.c
	EOF
	test_cmp expected out
	)
'

# ---------------------------------------------------------------------------
# Relative paths from subdirectory
# ---------------------------------------------------------------------------
test_expect_success 'ls-files from subdirectory shows relative paths' '
	(
	cd repo/src &&
	git ls-files >out &&
	cat >expected <<-EOF &&
	lib/core.c
	lib/helper.c
	main.c
	util.c
	EOF
	test_cmp expected out
	)
'

test_expect_success 'ls-files from nested subdirectory' '
	(
	cd repo/src/lib &&
	git ls-files >out &&
	cat >expected <<-EOF &&
	core.c
	helper.c
	EOF
	test_cmp expected out
	)
'

test_expect_success 'ls-files from subdirectory with parent reference' '
	(
	cd repo/src &&
	git ls-files ../docs/ >out &&
	grep -q "docs/api.md" out &&
	grep -q "docs/intro.md" out
	)
'

test_expect_success 'ls-files from subdirectory with exact parent file' '
	(
	cd repo/src &&
	git ls-files ../root.txt >out &&
	grep -q "root.txt" out
	)
'

# ---------------------------------------------------------------------------
# Staged flag (-s)
# ---------------------------------------------------------------------------
test_expect_success 'ls-files -s with pathspec shows mode and sha' '
	(
	cd repo &&
	git ls-files -s root.txt >out &&
	grep -q "100644" out &&
	grep -q "root.txt" out
	)
'

test_expect_success 'ls-files -s with directory pathspec' '
	(
	cd repo &&
	git ls-files -s src/lib/ >out &&
	line_count=$(wc -l <out | tr -d " ") &&
	test "$line_count" -eq 2
	)
'

# ---------------------------------------------------------------------------
# Case sensitivity
# ---------------------------------------------------------------------------
test_expect_success 'ls-files is case-sensitive by default' '
	(
	cd repo &&
	git ls-files Root.txt >out &&
	test_must_be_empty out
	)
'

test_expect_success 'ls-files exact case matches' '
	(
	cd repo &&
	git ls-files README.md >out &&
	echo "README.md" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'ls-files wrong case returns nothing' '
	(
	cd repo &&
	git ls-files readme.md >out &&
	test_must_be_empty out
	)
'

# ---------------------------------------------------------------------------
# No pathspec = all files
# ---------------------------------------------------------------------------
test_expect_success 'ls-files with no pathspec lists all files' '
	(
	cd repo &&
	git ls-files >out &&
	count=$(wc -l <out | tr -d " ") &&
	test "$count" -eq 10
	)
'

# ---------------------------------------------------------------------------
# Deleted files
# ---------------------------------------------------------------------------
test_expect_success 'ls-files -d shows deleted files' '
	(
	cd repo &&
	rm root.txt &&
	git ls-files -d >out &&
	grep -q "root.txt" out
	)
'

test_expect_success 'ls-files -d with pathspec' '
	(
	cd repo &&
	rm src/main.c &&
	git ls-files -d src/ >out &&
	grep -q "src/main.c" out
	)
'

test_expect_success 'restore deleted files' '
	(
	cd repo &&
	git checkout -- root.txt src/main.c
	)
'

# ---------------------------------------------------------------------------
# Modified files
# ---------------------------------------------------------------------------
test_expect_success 'ls-files -m shows modified files' '
	(
	cd repo &&
	echo "changed" >root.txt &&
	git ls-files -m >out &&
	grep -q "root.txt" out
	)
'

test_expect_success 'ls-files -m with directory pathspec' '
	(
	cd repo &&
	echo "changed" >src/main.c &&
	git ls-files -m src/ >out &&
	grep -q "src/main.c" out
	)
'

test_expect_success 'restore modified files' '
	(
	cd repo &&
	git checkout -- .
	)
'

# ---------------------------------------------------------------------------
# Others (untracked)
# ---------------------------------------------------------------------------
test_expect_success 'ls-files --cached is default' '
	(
	cd repo &&
	git ls-files --cached >out1 &&
	git ls-files >out2 &&
	test_cmp out1 out2
	)
'

test_expect_success 'ls-files -c is same as --cached' '
	(
	cd repo &&
	git ls-files -c >out1 &&
	git ls-files --cached >out2 &&
	test_cmp out1 out2
	)
'

test_expect_success 'ls-files with nonexistent directory returns empty' '
	(
	cd repo &&
	git ls-files nonexistent/ >out &&
	test_must_be_empty out
	)
'

test_done
