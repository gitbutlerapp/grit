#!/bin/sh
# Tests for grit check-ignore with various ignore patterns and flags.

test_description='grit check-ignore case sensitivity and pattern matching'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup: create repo with gitignore' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	cat >.gitignore <<-\EOF &&
	*.log
	*.tmp
	build/
	!important.log
	temp*
	*.o
	debug/
	EOF
	"$REAL_GIT" add .gitignore &&
	"$REAL_GIT" commit -m "add gitignore"
	)
'

###########################################################################
# Basic matching
###########################################################################

test_expect_success 'check-ignore matches *.log pattern' '
	(cd repo && grit check-ignore test.log >../actual) &&
	echo "test.log" >expect &&
	test_cmp expect actual
'

test_expect_success 'check-ignore does not match non-ignored file' '
	(cd repo && test_must_fail grit check-ignore test.txt)
'

test_expect_success 'check-ignore matches *.tmp pattern' '
	(cd repo && grit check-ignore data.tmp >../actual) &&
	echo "data.tmp" >expect &&
	test_cmp expect actual
'

test_expect_success 'check-ignore matches build/ directory pattern' '
	(cd repo && grit check-ignore build/output.o >../actual) &&
	echo "build/output.o" >expect &&
	test_cmp expect actual
'

test_expect_success 'check-ignore matches temp* prefix pattern' '
	(cd repo && grit check-ignore tempfile >../actual) &&
	echo "tempfile" >expect &&
	test_cmp expect actual
'

test_expect_success 'check-ignore matches *.o pattern' '
	(cd repo && grit check-ignore main.o >../actual) &&
	echo "main.o" >expect &&
	test_cmp expect actual
'

###########################################################################
# Negation patterns
###########################################################################

test_expect_success 'check-ignore respects negation !important.log' '
	(cd repo && test_must_fail grit check-ignore important.log)
'

test_expect_success 'check-ignore still ignores other .log files' '
	(cd repo && grit check-ignore other.log >../actual) &&
	echo "other.log" >expect &&
	test_cmp expect actual
'

###########################################################################
# Multiple arguments
###########################################################################

test_expect_success 'check-ignore with multiple args shows only ignored' '
	(cd repo && grit check-ignore test.log test.txt data.tmp >../actual) &&
	cat >expect <<-\EOF &&
	test.log
	data.tmp
	EOF
	test_cmp expect actual
'

test_expect_success 'check-ignore exits 0 when at least one matches' '
	(cd repo && grit check-ignore test.txt test.log)
'

test_expect_success 'check-ignore exits 1 when none match' '
	(cd repo && test_must_fail grit check-ignore test.txt readme.md)
'

###########################################################################
# -v / --verbose mode
###########################################################################

test_expect_success 'check-ignore -v shows source info' '
	(cd repo && grit check-ignore -v test.log >../actual) &&
	grep ".gitignore" actual &&
	grep "test.log" actual
'

test_expect_success 'check-ignore -v output has pattern field' '
	(cd repo && grit check-ignore -v test.log >../actual) &&
	grep "\*.log" actual
'

test_expect_success 'check-ignore -v matches git output' '
	(cd repo && grit check-ignore -v test.log >../actual) &&
	(cd repo && "$REAL_GIT" check-ignore -v test.log >../expect) &&
	test_cmp expect actual
'

test_expect_success 'check-ignore -v for directory pattern' '
	(cd repo && grit check-ignore -v build/output >../actual) &&
	grep "build/" actual
'

###########################################################################
# -v -n (verbose + non-matching)
###########################################################################

test_expect_success 'check-ignore -v -n shows non-matching files' '
	(cd repo && grit check-ignore -v -n test.txt >../actual || true) &&
	grep "test.txt" actual
'

test_expect_success 'check-ignore -v -n matches git for ignored file' '
	(cd repo && grit check-ignore -v -n test.log >../actual) &&
	(cd repo && "$REAL_GIT" check-ignore -v -n test.log >../expect) &&
	test_cmp expect actual
'

test_expect_success 'check-ignore -v -n matches git for non-ignored file' '
	(cd repo && grit check-ignore -v -n test.txt >../actual || true) &&
	(cd repo && "$REAL_GIT" check-ignore -v -n test.txt >../expect || true) &&
	test_cmp expect actual
'

###########################################################################
# --stdin mode
###########################################################################

test_expect_success 'check-ignore --stdin reads paths from stdin' '
	(cd repo && echo "test.log" | grit check-ignore --stdin >../actual) &&
	echo "test.log" >expect &&
	test_cmp expect actual
'

test_expect_success 'check-ignore --stdin with multiple lines' '
	(cd repo && printf "test.log\ntest.txt\ndata.tmp\n" | grit check-ignore --stdin >../actual) &&
	cat >expect <<-\EOF &&
	test.log
	data.tmp
	EOF
	test_cmp expect actual
'

test_expect_success 'check-ignore --stdin matches git' '
	(cd repo && echo "test.log" | grit check-ignore --stdin >../actual) &&
	(cd repo && echo "test.log" | "$REAL_GIT" check-ignore --stdin >../expect) &&
	test_cmp expect actual
'

###########################################################################
# -q / --quiet mode
###########################################################################

test_expect_success 'check-ignore -q produces no output on match' '
	(cd repo && grit check-ignore -q test.log >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'check-ignore -q returns 0 on match' '
	(cd repo && grit check-ignore -q test.log)
'

test_expect_success 'check-ignore -q returns 1 on no match' '
	(cd repo && test_must_fail grit check-ignore -q test.txt)
'

###########################################################################
# Subdirectory patterns
###########################################################################

test_expect_success 'setup: add subdirectory gitignore' '
	(cd repo &&
	 mkdir -p src &&
	 echo "*.bak" >src/.gitignore &&
	 "$REAL_GIT" add src/.gitignore &&
	 "$REAL_GIT" commit -m "add src gitignore")
'

test_expect_success 'check-ignore respects subdirectory gitignore' '
	(cd repo && grit check-ignore src/file.bak >../actual) &&
	echo "src/file.bak" >expect &&
	test_cmp expect actual
'

test_expect_success 'subdirectory gitignore does not affect parent' '
	(cd repo && test_must_fail grit check-ignore file.bak)
'

###########################################################################
# Edge cases
###########################################################################

test_expect_success 'check-ignore with no arguments fails' '
	(cd repo && test_must_fail grit check-ignore 2>/dev/null)
'

test_expect_success 'check-ignore debug/ pattern matches subpath' '
	(cd repo && grit check-ignore debug/test.c >../actual) &&
	echo "debug/test.c" >expect &&
	test_cmp expect actual
'

test_expect_success 'check-ignore matches git for complex case' '
	(cd repo && grit check-ignore -v main.o data.tmp tempfile >../actual) &&
	(cd repo && "$REAL_GIT" check-ignore -v main.o data.tmp tempfile >../expect) &&
	test_cmp expect actual
'

test_done
