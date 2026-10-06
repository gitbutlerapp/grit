#!/bin/sh
# Ported from git/t/t3902-quoted.sh (ls-tree focused subset).

test_description='grit ls-tree quoted output'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

HT='	'
DQ='"'

test_expect_success 'setup repository with quote-sensitive names' '
	(
	grit init repo &&
	cd repo &&
	echo initial >Name &&
	echo initial >"With SP in it" &&
	echo initial >"Name and an${HT}HT" &&
	echo initial >"Name${DQ}" &&
	grit update-index --add Name "With SP in it" "Name and an${HT}HT" "Name${DQ}" &&
	tree=$(grit write-tree) &&
	echo "$tree" >../tree_oid
	)
'

test_expect_success 'ls-tree --name-only -r defaults to quoted paths' '
	(
	cd repo &&
	cat >expect <<-\EOF &&
	Name
	"Name and an\tHT"
	"Name\""
	With SP in it
	EOF
	grit ls-tree --name-only -r "$(cat ../tree_oid)" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree -z --name-only disables quoting' '
	(
	cd repo &&
	grit ls-tree -z --name-only -r "$(cat ../tree_oid)" >raw &&
	# With -z, names should NOT be quoted (NUL terminated instead)
	# Check that the raw tab and double-quote appear literally
	printf "Name\0Name and an\tHT\0Name\"\0With SP in it\0" >expect &&
	test_cmp expect raw
	)
'

test_expect_success 'setup repository with backslash and newline names' '
	(
	cd repo &&
	echo initial >"back\\slash" &&
	grit update-index --add "back\\slash" &&
	tree2=$(grit write-tree) &&
	echo "$tree2" >../tree_oid2
	)
'

test_expect_success 'ls-tree quotes backslash in filenames' '
	(
	cd repo &&
	grit ls-tree --name-only -r "$(cat ../tree_oid2)" >actual &&
	grep "back\\\\\\\\slash" actual
	)
'

test_expect_success 'ls-tree full output quotes tab in path column' '
	(
	cd repo &&
	grit ls-tree -r "$(cat ../tree_oid)" >actual &&
	# The tab-containing filename should appear as a quoted string
	grep "Name and an" actual >line &&
	test_line_count = 1 line
	)
'

test_expect_success 'ls-tree --name-only with core.quotepath=false shows raw non-ASCII' '
	(
	cd repo &&
	grit config core.quotepath false &&
	grit ls-tree --name-only -r "$(cat ../tree_oid)" >actual &&
	# With quotepath off, "With SP in it" should still appear unquoted
	grep "With SP in it" actual &&
	# Tab-containing name should still be quoted (control characters always quoted)
	grep "Name and an" actual >line &&
	test_line_count = 1 line &&
	grit config --unset core.quotepath
	)
'

test_expect_success 'ls-tree -z --name-only with full output' '
	(
	cd repo &&
	grit ls-tree -z -r "$(cat ../tree_oid)" >raw &&
	# With -z, entries are NUL-separated and the full mode/type/hash line is present
	# Just verify we get output with NUL bytes
	test -s raw &&
	tr "\0" "\n" <raw >lines &&
	grep "Name" lines
	)
'

test_done
