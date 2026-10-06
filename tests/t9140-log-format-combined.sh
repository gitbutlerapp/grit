#!/bin/sh
# Test log --format with various format specifiers and combinations.

test_description='grit log --format combined format specifiers'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup repository with multiple commits' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "alice@example.com" &&
	git config user.name "Alice Author" &&
	echo "first" >file.txt &&
	grit add file.txt &&
	grit commit -m "first commit" &&
	echo "second" >>file.txt &&
	grit add file.txt &&
	grit commit -m "second commit" &&
	echo "third" >>file.txt &&
	grit add file.txt &&
	grit commit -m "third commit"
	)
'

###########################################################################
# Section 1: Individual format specifiers
###########################################################################

test_expect_success 'format %H shows full commit hash' '
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
	test ${#hash} -eq 7
	)
'

test_expect_success 'format %s shows subject line' '
	(
	cd repo &&
	grit log --format="%s" -n 1 >out &&
	grep "^third commit$" out
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

test_expect_success 'format %cn shows committer name' '
	(
	cd repo &&
	grit log --format="%cn" -n 1 >out &&
	grep "^C O Mitter$" out
	)
'

test_expect_success 'format %ce shows committer email' '
	(
	cd repo &&
	grit log --format="%ce" -n 1 >out &&
	grep "^committer@example.com$" out
	)
'

###########################################################################
# Section 2: Parent hash specifiers
###########################################################################

test_expect_success 'format %P shows full parent hash' '
	(
	cd repo &&
	grit log --format="%P" -n 1 >out &&
	parent=$(cat out) &&
	test ${#parent} -eq 40
	)
'

test_expect_success 'format %p shows abbreviated parent hash' '
	(
	cd repo &&
	grit log --format="%p" -n 1 >out &&
	parent=$(cat out) &&
	test ${#parent} -eq 7
	)
'

test_expect_success 'format %P on root commit is a known hash' '
	(
	cd repo &&
	grit log --format="%H %P" >all &&
	root_line=$(tail -1 all) &&
	root_hash=$(echo "$root_line" | cut -d" " -f1) &&
	grit log --format="%P" -n 1 $root_hash >out &&
	parent=$(cat out | tr -d " ") &&
	test -z "$parent" || test ${#parent} -eq 40
	)
'

###########################################################################
# Section 3: Tree hash specifiers
###########################################################################

test_expect_success 'format %T shows full tree hash' '
	(
	cd repo &&
	grit log --format="%T" -n 1 >out &&
	tree=$(cat out) &&
	test ${#tree} -eq 40
	)
'

test_expect_success 'format %t shows abbreviated tree hash' '
	(
	cd repo &&
	grit log --format="%t" -n 1 >out &&
	tree=$(cat out) &&
	test ${#tree} -eq 7
	)
'

###########################################################################
# Section 4: Combined format specifiers
###########################################################################

test_expect_success 'format with hash and subject combined' '
	(
	cd repo &&
	grit log --format="%h %s" -n 1 >out &&
	grep "third commit" out &&
	hash=$(cut -d" " -f1 <out) &&
	test ${#hash} -eq 7
	)
'

test_expect_success 'format with author name and email' '
	(
	cd repo &&
	grit log --format="%an <%ae>" -n 1 >out &&
	grep "^A U Thor <author@example.com>$" out
	)
'

test_expect_success 'format with pipe-separated fields' '
	(
	cd repo &&
	grit log --format="%H|%h|%an|%ae|%s" -n 1 >out &&
	fields=$(awk -F"|" "{print NF}" <out) &&
	test "$fields" = "5"
	)
'

test_expect_success 'format with hash, parent, tree combined' '
	(
	cd repo &&
	grit log --format="%H %P %T" -n 1 >out &&
	words=$(wc -w <out | tr -d " ") &&
	test "$words" = "3"
	)
'

###########################################################################
# Section 5: Multiple commits with format
###########################################################################

test_expect_success 'format %s shows all subjects' '
	(
	cd repo &&
	grit log --format="%s" >out &&
	grep "^third commit$" out &&
	grep "^second commit$" out &&
	grep "^first commit$" out
	)
'

test_expect_success 'format %H -n 2 limits to 2 commits' '
	(
	cd repo &&
	grit log --format="%H" -n 2 >out &&
	test_line_count = 2 out
	)
'

test_expect_success 'format %h for all commits' '
	(
	cd repo &&
	grit log --format="%h" >out &&
	test_line_count = 3 out
	)
'

###########################################################################
# Section 6: --oneline comparison
###########################################################################

test_expect_success 'oneline shows abbreviated hash and subject' '
	(
	cd repo &&
	grit log --oneline -n 1 >out &&
	grep "third commit" out
	)
'

test_expect_success 'oneline format matches %h %s roughly' '
	(
	cd repo &&
	grit log --oneline -n 1 >oneline_out &&
	hash_oneline=$(cut -d" " -f1 <oneline_out) &&
	grit log --format="%h" -n 1 >format_out &&
	hash_format=$(cat format_out) &&
	test "$hash_oneline" = "$hash_format"
	)
'

###########################################################################
# Section 7: Format with different committer
###########################################################################

test_expect_success 'setup commit with different author config' '
	(
	cd repo &&
	git config user.email "bob@example.com" &&
	git config user.name "Bob Builder" &&
	echo "fourth" >>file.txt &&
	grit add file.txt &&
	grit commit -m "bobs commit"
	)
'

test_expect_success 'format %an shows correct author per commit' '
	(
	cd repo &&
	grit log --format="%an" -n 1 >out &&
	grep "^A U Thor$" out
	)
'

test_expect_success 'format %ae shows correct email per commit' '
	(
	cd repo &&
	grit log --format="%ae" -n 1 >out &&
	grep "^author@example.com$" out
	)
'

test_expect_success 'format %an for older commit shows Alice' '
	(
	cd repo &&
	grit log --format="%an" --skip=1 -n 1 >out &&
	grep "^A U Thor$" out
	)
'

###########################################################################
# Section 8: Format with --reverse
###########################################################################

test_expect_success 'log --reverse reverses order' '
	(
	cd repo &&
	grit log --format="%s" --reverse >out &&
	head -1 out >first_line &&
	grep "^first commit$" first_line
	)
'

test_expect_success 'log --reverse --format=%h shows all in reverse' '
	(
	cd repo &&
	grit log --format="%h" --reverse >out &&
	test_line_count = 4 out
	)
'

###########################################################################
# Section 9: Format edge cases
###########################################################################

test_expect_success 'format with literal percent' '
	(
	cd repo &&
	grit log --format="%%" -n 1 >out &&
	grep "^%$" out
	)
'

test_expect_success 'format with newline specifier' '
	(
	cd repo &&
	grit log --format="%H%n%s" -n 1 >out &&
	test_line_count = 2 out
	)
'

###########################################################################
# Section 10: Format with --skip and --max-count
###########################################################################

test_expect_success 'log --skip=2 --format=%s skips first two' '
	(
	cd repo &&
	grit log --skip=2 --format="%s" >out &&
	! grep "bobs commit" out &&
	! grep "third commit" out &&
	grep "second commit" out &&
	grep "first commit" out
	)
'

test_expect_success 'log -n 1 --skip=1 --format=%s shows second commit' '
	(
	cd repo &&
	grit log -n 1 --skip=1 --format="%s" >out &&
	grep "^third commit$" out
	)
'

test_done
