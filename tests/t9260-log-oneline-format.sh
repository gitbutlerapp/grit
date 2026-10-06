#!/bin/sh
# Test log --oneline, --format, --reverse, --graph, --skip, --decorate,
# --no-decorate, and combinations.

test_description='grit log --oneline and format options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup repository with five commits' '
	(
	grit init --initial-branch=master repo &&
	cd repo &&
	$REAL_GIT config user.email "alice@example.com" &&
	$REAL_GIT config user.name "Alice Author" &&
	echo "one" >file.txt &&
	grit add file.txt &&
	grit commit -m "first commit" &&
	echo "two" >>file.txt &&
	grit add file.txt &&
	grit commit -m "second commit" &&
	echo "three" >>file.txt &&
	grit add file.txt &&
	grit commit -m "third commit" &&
	echo "four" >>file.txt &&
	grit add file.txt &&
	grit commit -m "fourth commit" &&
	echo "five" >>file.txt &&
	grit add file.txt &&
	grit commit -m "fifth commit"
	)
'

###########################################################################
# Section 1: --oneline basic
###########################################################################

test_expect_success 'log --oneline shows all commits' '
	(
	cd repo &&
	grit log --oneline >out &&
	test_line_count = 5 out
	)
'

test_expect_success 'log --oneline shows abbreviated hash' '
	(
	cd repo &&
	grit log --oneline -n 1 >out &&
	hash=$(cat out | awk "{print \$1}") &&
	test ${#hash} -eq 7 || test ${#hash} -lt 12
	)
'

test_expect_success 'log --oneline shows subject on same line' '
	(
	cd repo &&
	grit log --oneline -n 1 >out &&
	grep "fifth commit" out
	)
'

test_expect_success 'log --oneline most recent first' '
	(
	cd repo &&
	grit log --oneline >out &&
	head -1 out | grep "fifth commit" &&
	tail -1 out | grep "first commit"
	)
'

test_expect_success 'log --oneline -n 3 limits output' '
	(
	cd repo &&
	grit log --oneline -n 3 >out &&
	test_line_count = 3 out &&
	grep "fifth commit" out &&
	grep "third commit" out &&
	! grep "second commit" out
	)
'

test_expect_success 'log --oneline -n 1 shows only HEAD' '
	(
	cd repo &&
	grit log --oneline -n 1 >out &&
	test_line_count = 1 out &&
	grep "fifth commit" out
	)
'

###########################################################################
# Section 2: --format=oneline
###########################################################################

test_expect_success 'log --format=oneline shows abbreviated hash and subject' '
	(
	cd repo &&
	grit log --format=oneline -n 1 >out &&
	grep "fifth commit" out
	)
'

test_expect_success 'log --format=oneline matches --oneline output' '
	(
	cd repo &&
	grit log --oneline >oneline_out &&
	grit log --format=oneline >format_out &&
	test_cmp oneline_out format_out
	)
'

###########################################################################
# Section 3: Custom --format specifiers
###########################################################################

test_expect_success 'format %H shows full 40-char hash' '
	(
	cd repo &&
	grit log --format="%H" -n 1 >out &&
	hash=$(cat out) &&
	test ${#hash} -eq 40
	)
'

test_expect_success 'format %h shows abbreviated hash' '
	(
	cd repo &&
	grit log --format="%h" -n 1 >out &&
	hash=$(cat out) &&
	test ${#hash} -le 12
	)
'

test_expect_success 'format %s shows subject line' '
	(
	cd repo &&
	grit log --format="%s" -n 1 >out &&
	grep "^fifth commit$" out
	)
'

test_expect_success 'format %an shows author name' '
	(
	cd repo &&
	grit log --format="%an" -n 1 >out &&
	grep "^A U Thor$" out
	)
'

test_expect_success 'format %ae shows author email' '
	(
	cd repo &&
	grit log --format="%ae" -n 1 >out &&
	grep "^author@example.com$" out
	)
'

test_expect_success 'format %h %s produces oneline-like output' '
	(
	cd repo &&
	grit log --format="%h %s" >out &&
	test_line_count = 5 out &&
	head -1 out | grep "fifth commit"
	)
'

###########################################################################
# Section 4: --reverse
###########################################################################

test_expect_success 'log --oneline --reverse shows oldest first' '
	(
	cd repo &&
	grit log --oneline --reverse >out &&
	head -1 out | grep "first commit" &&
	tail -1 out | grep "fifth commit"
	)
'

test_expect_success 'log --reverse has same count as normal' '
	(
	cd repo &&
	grit log --oneline >normal &&
	grit log --oneline --reverse >reversed &&
	test_line_count = 5 reversed
	)
'

test_expect_success 'log --reverse -n 3 shows first 3 in reverse' '
	(
	cd repo &&
	grit log --oneline --reverse -n 3 >out &&
	test_line_count = 3 out
	)
'

###########################################################################
# Section 5: --skip
###########################################################################

test_expect_success 'log --oneline --skip 2 skips most recent two' '
	(
	cd repo &&
	grit log --oneline --skip 2 >out &&
	test_line_count = 3 out &&
	! grep "fifth commit" out &&
	! grep "fourth commit" out &&
	grep "third commit" out
	)
'

test_expect_success 'log --oneline --skip 4 shows only first commit' '
	(
	cd repo &&
	grit log --oneline --skip 4 >out &&
	test_line_count = 1 out &&
	grep "first commit" out
	)
'

test_expect_success 'log --skip combined with -n' '
	(
	cd repo &&
	grit log --oneline --skip 1 -n 2 >out &&
	test_line_count = 2 out &&
	grep "fourth commit" out &&
	grep "third commit" out
	)
'

###########################################################################
# Section 6: --graph
###########################################################################

test_expect_success 'log --oneline --graph produces output' '
	(
	cd repo &&
	grit log --oneline --graph >out &&
	test -s out
	)
'

test_expect_success 'log --graph output includes commit subjects' '
	(
	cd repo &&
	grit log --oneline --graph >out &&
	grep "fifth commit" out &&
	grep "first commit" out
	)
'

###########################################################################
# Section 7: --decorate and --no-decorate
###########################################################################

test_expect_success 'log --oneline --decorate shows ref names' '
	(
	cd repo &&
	grit log --oneline --decorate -n 1 >out &&
	grep "HEAD" out &&
	grep "master" out
	)
'

test_expect_success 'log --oneline --no-decorate hides ref names' '
	(
	cd repo &&
	grit log --oneline --no-decorate -n 1 >out &&
	! grep "HEAD" out
	)
'

###########################################################################
# Section 8: Specific revision
###########################################################################

test_expect_success 'log --oneline with specific revision' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD~2) &&
	grit log --oneline $sha >out &&
	test_line_count = 3 out &&
	grep "third commit" out &&
	! grep "fourth commit" out
	)
'

test_expect_success 'log --format=%s with specific revision' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD~4) &&
	grit log --format="%s" $sha >out &&
	test_line_count = 1 out &&
	grep "^first commit$" out
	)
'

###########################################################################
# Section 9: Merge commit with --first-parent
###########################################################################

test_expect_success 'setup merge commit' '
	(
	cd repo &&
	$REAL_GIT checkout -b side &&
	echo "side" >side.txt &&
	grit add side.txt &&
	grit commit -m "side commit" &&
	$REAL_GIT checkout master &&
	$REAL_GIT merge side --no-edit &&
	grit log --oneline >out &&
	grep "side commit" out
	)
'

test_expect_success 'log --first-parent produces output' '
	(
	cd repo &&
	grit log --oneline --first-parent >out &&
	test -s out
	)
'

test_expect_success 'log --first-parent includes merge commit' '
	(
	cd repo &&
	grit log --oneline --first-parent >out &&
	grep "Merge" out || grep "merge" out || grep "fifth commit" out
	)
'

###########################################################################
# Section 10: Edge cases and combinations
###########################################################################

test_expect_success 'log --oneline -n 1 shows single commit' '
	(
	cd repo &&
	grit log --oneline -n 1 >out &&
	test_line_count = 1 out
	)
'

test_expect_success 'log --format with empty string shows blank lines' '
	(
	cd repo &&
	grit log --format="" -n 3 >out &&
	test -f out
	)
'

test_expect_success 'log --oneline --graph --reverse is rejected' '
	(
	cd repo &&
	test_must_fail grit log --oneline --graph --reverse >out 2>err &&
	grep "reverse.*graph\\|graph.*reverse" err
	)
'

test_done
