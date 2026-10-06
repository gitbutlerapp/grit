#!/bin/sh
# Tests for pathspec wildcard matching in ls-files, diff, and log.
# grit does not yet support glob expansion in pathspecs, so all wildcard
# tests are marked as expected failures to document the desired behavior
# and track progress toward implementation.

test_description='pathspec wildcard matching'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with varied file extensions' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	mkdir -p src lib doc imgs &&
	echo "a" >src/main.c &&
	echo "b" >src/util.c &&
	echo "c" >src/main.h &&
	echo "d" >src/util.h &&
	echo "e" >src/build.rs &&
	echo "f" >lib/core.c &&
	echo "g" >lib/core.h &&
	echo "h" >lib/extra.c &&
	echo "i" >doc/readme.md &&
	echo "j" >doc/notes.md &&
	echo "k" >doc/todo.txt &&
	echo "l" >imgs/logo.png &&
	echo "m" >imgs/icon.png &&
	echo "n" >imgs/banner.jpg &&
	echo "o" >Makefile &&
	echo "p" >README.md &&
	echo "q" >LICENSE &&
	echo "r" >src/config.toml &&
	git add . &&
	git commit -m "initial files"
	)
'

# ── Star wildcard with extensions ────────────────────────────────────────────

test_expect_success 'ls-files *.c matches all C files recursively' '
	(
	cd repo &&
	git ls-files "*.c" >../actual &&
	grep "src/main.c" ../actual &&
	grep "src/util.c" ../actual &&
	grep "lib/core.c" ../actual &&
	grep "lib/extra.c" ../actual &&
	test_line_count = 4 ../actual
	)
'

test_expect_success 'ls-files *.h matches all header files' '
	(
	cd repo &&
	git ls-files "*.h" >../actual &&
	grep "src/main.h" ../actual &&
	grep "src/util.h" ../actual &&
	grep "lib/core.h" ../actual &&
	test_line_count = 3 ../actual
	)
'

test_expect_success 'ls-files *.md matches all markdown files' '
	(
	cd repo &&
	git ls-files "*.md" >../actual &&
	grep "doc/readme.md" ../actual &&
	grep "doc/notes.md" ../actual &&
	grep "README.md" ../actual &&
	test_line_count = 3 ../actual
	)
'

test_expect_success 'ls-files *.png matches PNG files' '
	(
	cd repo &&
	git ls-files "*.png" >../actual &&
	test_line_count = 2 ../actual
	)
'

test_expect_success 'ls-files *.jpg matches JPG files' '
	(
	cd repo &&
	git ls-files "*.jpg" >../actual &&
	test_line_count = 1 ../actual
	)
'

test_expect_success 'ls-files *.rs matches Rust files' '
	(
	cd repo &&
	git ls-files "*.rs" >../actual &&
	test_line_count = 1 ../actual
	)
'

test_expect_success 'ls-files *.toml matches TOML files' '
	(
	cd repo &&
	git ls-files "*.toml" >../actual &&
	test_line_count = 1 ../actual
	)
'

test_expect_success 'ls-files *.xyz matches nothing' '
	(
	cd repo &&
	git ls-files "*.xyz" >../actual &&
	test_must_be_empty ../actual
	)
'

# ── Star wildcard with directory prefix ──────────────────────────────────────

test_expect_success 'ls-files src/*.c matches only src C files' '
	(
	cd repo &&
	git ls-files "src/*.c" >../actual &&
	test_line_count = 2 ../actual &&
	grep "src/main.c" ../actual &&
	grep "src/util.c" ../actual
	)
'

test_expect_success 'ls-files src/*.h matches only src headers' '
	(
	cd repo &&
	git ls-files "src/*.h" >../actual &&
	test_line_count = 2 ../actual
	)
'

test_expect_success 'ls-files lib/*.c matches only lib C files' '
	(
	cd repo &&
	git ls-files "lib/*.c" >../actual &&
	test_line_count = 2 ../actual
	)
'

test_expect_success 'ls-files doc/*.md matches only doc markdown' '
	(
	cd repo &&
	git ls-files "doc/*.md" >../actual &&
	test_line_count = 2 ../actual
	)
'

test_expect_success 'ls-files imgs/* matches all image files' '
	(
	cd repo &&
	git ls-files "imgs/*" >../actual &&
	test_line_count = 3 ../actual
	)
'

# ── Question mark wildcard ──────────────────────────────────────────────────

test_expect_success 'ls-files src/main.? matches main.c and main.h' '
	(
	cd repo &&
	git ls-files "src/main.?" >../actual &&
	test_line_count = 2 ../actual &&
	grep "src/main.c" ../actual &&
	grep "src/main.h" ../actual
	)
'

test_expect_success 'ls-files src/util.? matches util.c and util.h' '
	(
	cd repo &&
	git ls-files "src/util.?" >../actual &&
	test_line_count = 2 ../actual
	)
'

test_expect_success 'ls-files *.?? matches two-char extensions' '
	(
	cd repo &&
	git ls-files "*.??" >../actual &&
	grep "src/build.rs" ../actual &&
	grep "doc/readme.md" ../actual
	)
'

# ── Bracket character class ─────────────────────────────────────────────────

test_expect_success 'ls-files src/*.[ch] matches C and header files' '
	(
	cd repo &&
	git ls-files "src/*.[ch]" >../actual &&
	test_line_count = 4 ../actual
	)
'

test_expect_success 'ls-files lib/*.[ch] matches lib C and headers' '
	(
	cd repo &&
	git ls-files "lib/*.[ch]" >../actual &&
	test_line_count = 3 ../actual
	)
'

test_expect_success 'ls-files imgs/*.[pj]?? matches png and jpg' '
	(
	cd repo &&
	git ls-files "imgs/*.[pj]??" >../actual &&
	test_line_count = 3 ../actual
	)
'

# ── Wildcard from subdirectory ──────────────────────────────────────────────

test_expect_success 'ls-files *.c from src shows local C files' '
	(
	cd repo/src &&
	git ls-files "*.c" >../../actual &&
	test_line_count = 2 ../../actual &&
	grep "main.c" ../../actual &&
	grep "util.c" ../../actual
	)
'

test_expect_success 'ls-files *.h from src shows local headers' '
	(
	cd repo/src &&
	git ls-files "*.h" >../../actual &&
	test_line_count = 2 ../../actual
	)
'

test_expect_success 'ls-files *.c from lib shows lib C files' '
	(
	cd repo/lib &&
	git ls-files "*.c" >../../actual &&
	test_line_count = 2 ../../actual
	)
'

test_expect_success 'ls-files *.md from doc shows doc markdown' '
	(
	cd repo/doc &&
	git ls-files "*.md" >../../actual &&
	test_line_count = 2 ../../actual
	)
'

# ── Multiple wildcards combined ─────────────────────────────────────────────

test_expect_success 'ls-files with multiple wildcard pathspecs' '
	(
	cd repo &&
	git ls-files "*.c" "*.h" >../actual &&
	test_line_count = 7 ../actual
	)
'

test_expect_success 'ls-files with file and wildcard pathspecs' '
	(
	cd repo &&
	git ls-files "Makefile" "*.md" >../actual &&
	test_line_count = 4 ../actual
	)
'

# ── Wildcard only shows tracked files ───────────────────────────────────────

test_expect_success 'ls-files wildcard only shows tracked (not untracked)' '
	(
	cd repo &&
	echo "untracked" >src/temp.c &&
	git ls-files "src/*.c" >../actual &&
	test_line_count = 2 ../actual &&
	! grep "temp.c" ../actual &&
	rm -f src/temp.c
	)
'

test_expect_success 'ls-files -o with wildcard shows untracked' '
	(
	cd repo &&
	echo "untracked" >src/temp.c &&
	git ls-files -o "src/*.c" >../actual &&
	test_line_count = 1 ../actual &&
	grep "temp.c" ../actual &&
	rm -f src/temp.c
	)
'

# ── diff/log with wildcard pathspec (expected failures) ─────────────────────

test_expect_success 'setup: modify files for diff wildcard tests' '
	(
	cd repo &&
	echo "modified" >>src/main.c &&
	echo "modified" >>src/main.h &&
	echo "modified" >>lib/core.c
	)
'

test_expect_success 'diff --name-only -- *.c with wildcard pathspec' '
	(
	cd repo &&
	git diff --name-only -- "*.c" >../actual &&
	test_line_count = 2 ../actual &&
	grep "src/main.c" ../actual &&
	grep "lib/core.c" ../actual
	)
'

test_expect_success 'diff --name-only -- src/*.? with wildcard' '
	(
	cd repo &&
	git diff --name-only -- "src/*.?" >../actual &&
	test_line_count = 2 ../actual
	)
'

test_expect_success 'log --oneline -- *.md with wildcard pathspec' '
	(
	cd repo &&
	git log --oneline -- "*.md" >../actual &&
	test_line_count = 1 ../actual
	)
'

test_done
