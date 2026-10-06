#!/bin/sh
# Tests for grit check-ignore with global gitignore, nested ignores, verbose, stdin, negation.

test_description='grit check-ignore: global patterns, local .gitignore, verbose, stdin, negation, directory rules'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with gitignore files' '
	(
	"$REAL_GIT" init -b main repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&

	cat >.gitignore <<-EOF &&
	*.log
	*.tmp
	build/
	!important.log
	EOF

	mkdir -p src &&
	cat >src/.gitignore <<-EOF &&
	*.o
	*.d
	EOF

	mkdir -p build &&
	mkdir -p docs &&

	echo "code" >src/main.c &&
	echo "readme" >docs/readme.md &&
	"$REAL_GIT" add .gitignore src/.gitignore src/main.c docs/readme.md &&
	"$REAL_GIT" commit -m "initial with gitignore"
	)
'

###########################################################################
# Section 2: Basic check-ignore
###########################################################################

test_expect_success 'check-ignore: ignored file is reported' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore debug.log >output.txt &&
	grep "debug.log" output.txt
	)
'

test_expect_success 'check-ignore: non-ignored file returns non-zero' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" check-ignore src/main.c
	)
'

test_expect_success 'check-ignore: *.tmp matched' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore temp.tmp >output.txt &&
	grep "temp.tmp" output.txt
	)
'

test_expect_success 'check-ignore: build/ directory matched' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore build/output.bin >output.txt &&
	grep "build/output.bin" output.txt
	)
'

test_expect_success 'check-ignore: tracked file in non-ignored dir not ignored' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" check-ignore docs/readme.md
	)
'

###########################################################################
# Section 3: Negation patterns
###########################################################################

test_expect_success 'check-ignore: negated pattern (important.log) not ignored' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" check-ignore important.log
	)
'

test_expect_success 'check-ignore: other .log files still ignored' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore error.log >output.txt &&
	grep "error.log" output.txt
	)
'

###########################################################################
# Section 4: Subdirectory gitignore
###########################################################################

test_expect_success 'check-ignore: src/.gitignore pattern *.o' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore src/main.o >output.txt &&
	grep "src/main.o" output.txt
	)
'

test_expect_success 'check-ignore: src/.gitignore pattern *.d' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore src/main.d >output.txt &&
	grep "src/main.d" output.txt
	)
'

test_expect_success 'check-ignore: *.o not ignored outside src/' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" check-ignore main.o
	)
'

test_expect_success 'check-ignore: root pattern applies in subdirectory' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore docs/notes.log >output.txt &&
	grep "docs/notes.log" output.txt
	)
'

###########################################################################
# Section 5: Verbose mode (-v)
###########################################################################

test_expect_success 'check-ignore -v: shows source file and pattern' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore -v debug.log >output.txt &&
	grep ".gitignore" output.txt &&
	grep "\\*.log" output.txt
	)
'

test_expect_success 'check-ignore -v: shows line number' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore -v debug.log >output.txt &&
	# Format: source:linenum:pattern\tpathname
	grep ":[0-9]*:" output.txt
	)
'

test_expect_success 'check-ignore -v: subdirectory pattern shows correct source' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore -v src/main.o >output.txt &&
	grep "src/.gitignore" output.txt
	)
'

###########################################################################
# Section 6: Multiple paths
###########################################################################

test_expect_success 'check-ignore: multiple paths, some ignored some not' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore debug.log src/main.c temp.tmp >output.txt 2>&1 || true &&
	grep "debug.log" output.txt &&
	grep "temp.tmp" output.txt
	)
'

test_expect_success 'check-ignore: all ignored paths reported' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore a.log b.log c.tmp >output.txt &&
	test_line_count = 3 output.txt
	)
'

test_expect_success 'check-ignore: all non-ignored paths return non-zero' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" check-ignore src/main.c docs/readme.md
	)
'

###########################################################################
# Section 7: --stdin
###########################################################################

test_expect_success 'check-ignore --stdin: read paths from stdin' '
	(
	cd repo &&
	printf "debug.log\ntemp.tmp\n" | "$GUST_BIN" check-ignore --stdin >output.txt &&
	grep "debug.log" output.txt &&
	grep "temp.tmp" output.txt
	)
'

test_expect_success 'check-ignore --stdin: mixed ignored and non-ignored' '
	(
	cd repo &&
	printf "debug.log\nsrc/main.c\n" | "$GUST_BIN" check-ignore --stdin >output.txt 2>&1 || true &&
	grep "debug.log" output.txt
	)
'

test_expect_success 'check-ignore --stdin -v: verbose with stdin' '
	(
	cd repo &&
	printf "debug.log\n" | "$GUST_BIN" check-ignore --stdin -v >output.txt &&
	grep ".gitignore" output.txt
	)
'

###########################################################################
# Section 8: -z (NUL-terminated)
###########################################################################

test_expect_success 'check-ignore --stdin -z: NUL-terminated input' '
	(
	cd repo &&
	printf "debug.log\0temp.tmp\0" | "$GUST_BIN" check-ignore --stdin -z >output.txt &&
	test -s output.txt
	)
'

###########################################################################
# Section 9: Global gitignore
###########################################################################

test_expect_success 'setup: create global gitignore' '
	(
	cd repo &&
	cat >$HOME/.gitignore_global <<-EOF &&
	*.swp
	*.swo
	*~
	.DS_Store
	EOF
	"$REAL_GIT" config core.excludesFile "$HOME/.gitignore_global"
	)
'

test_expect_success 'check-ignore: global gitignore pattern *.swp' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore file.swp >output.txt &&
	grep "file.swp" output.txt
	)
'

test_expect_success 'check-ignore: global gitignore pattern .DS_Store' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore .DS_Store >output.txt &&
	grep ".DS_Store" output.txt
	)
'

test_expect_success 'check-ignore: global gitignore *~' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore backup~ >output.txt &&
	grep "backup~" output.txt
	)
'

test_expect_success 'check-ignore -v: shows global excludes file' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore -v file.swp >output.txt &&
	grep "gitignore_global" output.txt
	)
'

###########################################################################
# Section 10: Directory patterns
###########################################################################

test_expect_success 'check-ignore: directory trailing slash pattern' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore build/subdir/file.txt >output.txt &&
	grep "build/subdir/file.txt" output.txt
	)
'

test_expect_success 'check-ignore: nested path under ignored directory' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore build/a/b/c/deep.txt >output.txt &&
	grep "build/a/b/c/deep.txt" output.txt
	)
'

###########################################################################
# Section 11: .git/info/exclude
###########################################################################

test_expect_success 'setup: add .git/info/exclude pattern' '
	(
	cd repo &&
	mkdir -p .git/info &&
	echo "secret.key" >.git/info/exclude
	)
'

test_expect_success 'check-ignore: .git/info/exclude pattern matched' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore secret.key >output.txt &&
	grep "secret.key" output.txt
	)
'

test_expect_success 'check-ignore -v: shows info/exclude source' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore -v secret.key >output.txt &&
	grep "exclude" output.txt
	)
'

###########################################################################
# Section 12: -n / --non-matching
###########################################################################

test_expect_success 'check-ignore: multiple ignored files counted correctly' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore debug.log error.log app.tmp >output.txt &&
	test_line_count = 3 output.txt
	)
'

test_expect_success 'check-ignore -v: global pattern shows line and pattern' '
	(
	cd repo &&
	"$GUST_BIN" check-ignore -v .DS_Store >output.txt &&
	grep ".DS_Store" output.txt &&
	grep "gitignore_global" output.txt
	)
'

test_done
