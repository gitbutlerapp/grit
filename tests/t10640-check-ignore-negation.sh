#!/bin/sh
# Tests for grit check-ignore with negation patterns and various flags.

test_description='grit check-ignore negation patterns'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with gitignore' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "initial" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial commit"
	)
'

###########################################################################
# Section 2: Basic ignore patterns
###########################################################################

test_expect_success 'setup: create .gitignore with patterns' '
	(
	cd repo &&
	cat >.gitignore <<-\EOF &&
	*.log
	*.tmp
	build/
	!important.log
	!keep.tmp
	EOF
	"$REAL_GIT" add .gitignore &&
	"$REAL_GIT" commit -m "add gitignore"
	)
'

test_expect_success 'check-ignore matches *.log pattern' '
	(
	cd repo &&
	grit check-ignore test.log >actual &&
	echo "test.log" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'check-ignore matches *.log same as git' '
	(
	cd repo &&
	grit check-ignore test.log >actual &&
	"$REAL_GIT" check-ignore test.log >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'check-ignore matches *.tmp pattern' '
	(
	cd repo &&
	grit check-ignore scratch.tmp >actual &&
	echo "scratch.tmp" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'check-ignore matches build/ directory' '
	(
	cd repo &&
	grit check-ignore build/output.o >actual &&
	echo "build/output.o" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'check-ignore on unmatched file exits non-zero' '
	(
	cd repo &&
	test_must_fail grit check-ignore file.txt
	)
'

test_expect_success 'check-ignore on unmatched file produces no output' '
	(
	cd repo &&
	grit check-ignore file.txt >actual 2>/dev/null || true &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 3: Negation patterns
###########################################################################

test_expect_success 'negation: !important.log is NOT ignored' '
	(
	cd repo &&
	test_must_fail grit check-ignore important.log
	)
'

test_expect_success 'negation: !important.log matches git behavior' '
	(
	cd repo &&
	grit check-ignore important.log >actual 2>/dev/null || true &&
	"$REAL_GIT" check-ignore important.log >expect 2>/dev/null || true &&
	test_cmp expect actual
	)
'

test_expect_success 'negation: !keep.tmp is NOT ignored' '
	(
	cd repo &&
	test_must_fail grit check-ignore keep.tmp
	)
'

test_expect_success 'negation: other .log files still ignored' '
	(
	cd repo &&
	grit check-ignore debug.log >actual &&
	echo "debug.log" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'negation: other .tmp files still ignored' '
	(
	cd repo &&
	grit check-ignore session.tmp >actual &&
	echo "session.tmp" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 4: --verbose (-v) output
###########################################################################

test_expect_success 'check-ignore -v shows source info' '
	(
	cd repo &&
	grit check-ignore -v test.log >actual &&
	"$REAL_GIT" check-ignore -v test.log >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'check-ignore -v output contains pattern' '
	(
	cd repo &&
	grit check-ignore -v test.log >actual &&
	grep "\\*.log" actual
	)
'

test_expect_success 'check-ignore -v output contains .gitignore filename' '
	(
	cd repo &&
	grit check-ignore -v test.log >actual &&
	grep "\\.gitignore" actual
	)
'

test_expect_success 'check-ignore -v for build/ pattern' '
	(
	cd repo &&
	grit check-ignore -v build/file.o >actual &&
	"$REAL_GIT" check-ignore -v build/file.o >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 5: --verbose --non-matching (-v -n)
###########################################################################

test_expect_success 'check-ignore -v -n shows negated patterns' '
	(
	cd repo &&
	grit check-ignore -v -n important.log >actual &&
	"$REAL_GIT" check-ignore -v -n important.log >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'check-ignore -v -n shows negation entry' '
	(
	cd repo &&
	grit check-ignore -v -n important.log >actual &&
	grep "!important.log" actual
	)
'

test_expect_success 'check-ignore -v -n for unmatched file shows empty source' '
	(
	cd repo &&
	grit check-ignore -v -n unmatched.txt >actual || true &&
	"$REAL_GIT" check-ignore -v -n unmatched.txt >expect || true &&
	test_cmp expect actual
	)
'

test_expect_success 'check-ignore -v -n for ignored file matches -v' '
	(
	cd repo &&
	grit check-ignore -v -n test.log >actual &&
	"$REAL_GIT" check-ignore -v -n test.log >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 6: Multiple paths
###########################################################################

test_expect_success 'check-ignore with multiple paths' '
	(
	cd repo &&
	grit check-ignore test.log debug.log >actual &&
	"$REAL_GIT" check-ignore test.log debug.log >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'check-ignore mixed ignored and non-ignored' '
	(
	cd repo &&
	grit check-ignore test.log file.txt important.log >actual &&
	"$REAL_GIT" check-ignore test.log file.txt important.log >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'check-ignore multiple paths exit code reflects any match' '
	(
	cd repo &&
	grit check-ignore file.txt test.log
	)
'

test_expect_success 'check-ignore all non-matching exits non-zero' '
	(
	cd repo &&
	test_must_fail grit check-ignore file.txt important.log
	)
'

###########################################################################
# Section 7: --no-index
###########################################################################

test_expect_success 'check-ignore --no-index works same as without for untracked' '
	(
	cd repo &&
	grit check-ignore --no-index test.log >actual &&
	grit check-ignore test.log >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'check-ignore --no-index with -v' '
	(
	cd repo &&
	grit check-ignore --no-index -v test.log >actual &&
	"$REAL_GIT" check-ignore --no-index -v test.log >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 8: Subdirectory .gitignore
###########################################################################

test_expect_success 'setup: create subdirectory with local .gitignore' '
	(
	cd repo &&
	mkdir -p subdir &&
	cat >subdir/.gitignore <<-\EOF &&
	*.dat
	!special.dat
	EOF
	"$REAL_GIT" add subdir/.gitignore &&
	"$REAL_GIT" commit -m "add subdir gitignore"
	)
'

test_expect_success 'subdirectory .gitignore pattern matches' '
	(
	cd repo &&
	grit check-ignore subdir/data.dat >actual &&
	echo "subdir/data.dat" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'subdirectory negation works' '
	(
	cd repo &&
	test_must_fail grit check-ignore subdir/special.dat
	)
'

test_expect_success 'subdirectory pattern does not apply to root' '
	(
	cd repo &&
	test_must_fail grit check-ignore data.dat
	)
'

test_expect_success 'subdirectory -v matches git' '
	(
	cd repo &&
	grit check-ignore -v subdir/data.dat >actual &&
	"$REAL_GIT" check-ignore -v subdir/data.dat >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 9: Patterns with slashes and wildcards
###########################################################################

test_expect_success 'setup: add complex patterns' '
	(
	cd repo &&
	cat >>.gitignore <<-\EOF &&
	doc/*.html
	**/temp/
	!doc/index.html
	EOF
	"$REAL_GIT" add .gitignore &&
	"$REAL_GIT" commit -m "add complex patterns"
	)
'

test_expect_success 'doc/*.html matches doc/readme.html' '
	(
	cd repo &&
	grit check-ignore doc/readme.html >actual &&
	echo "doc/readme.html" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'doc/*.html negation: doc/index.html not ignored' '
	(
	cd repo &&
	test_must_fail grit check-ignore doc/index.html
	)
'

test_expect_success '**/temp/ matches nested temp directories' '
	(
	cd repo &&
	grit check-ignore a/b/temp/file.txt >actual &&
	echo "a/b/temp/file.txt" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '**/temp/ matches temp/ at various depths' '
	(
	cd repo &&
	grit check-ignore x/temp/file.txt >actual &&
	echo "x/temp/file.txt" >expect &&
	test_cmp expect actual
	)
'

test_done
