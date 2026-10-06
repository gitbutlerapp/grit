#!/bin/sh
# Tests for grit ls-tree with recursive mode and various options.

test_description='grit ls-tree recursive'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with nested tree' '
	(
	grit init repo &&
	cd repo &&
	echo "root file" >root.txt &&
	mkdir -p dir1/sub1 dir2 &&
	echo "dir1 file" >dir1/file1.txt &&
	echo "sub1 file" >dir1/sub1/deep.txt &&
	echo "dir2 file" >dir2/file2.txt &&
	echo "another" >dir2/another.txt &&
	grit add . &&
	tree=$(grit write-tree) &&
	echo "$tree" >../tree_oid
	)
'

###########################################################################
# Section 2: Non-recursive listing
###########################################################################

test_expect_success 'ls-tree without -r shows top-level entries' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree "$tree" >actual &&
	grep "root.txt" actual &&
	grep "dir1" actual &&
	grep "dir2" actual
	)
'

test_expect_success 'ls-tree without -r does not show nested files' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree "$tree" >actual &&
	! grep "file1.txt" actual &&
	! grep "deep.txt" actual
	)
'

test_expect_success 'ls-tree shows trees as 040000 type' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree "$tree" | grep "dir1" >actual &&
	grep "^040000 tree" actual
	)
'

test_expect_success 'ls-tree shows blobs as 100644 type' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree "$tree" | grep "root.txt" >actual &&
	grep "^100644 blob" actual
	)
'

test_expect_success 'ls-tree top-level entry count is correct' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree "$tree" >actual &&
	test_line_count = 3 actual
	)
'

###########################################################################
# Section 3: Recursive listing
###########################################################################

test_expect_success 'ls-tree -r shows all files recursively' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r "$tree" >actual &&
	grep "root.txt" actual &&
	grep "dir1/file1.txt" actual &&
	grep "dir1/sub1/deep.txt" actual &&
	grep "dir2/file2.txt" actual &&
	grep "dir2/another.txt" actual
	)
'

test_expect_success 'ls-tree -r shows correct total file count' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r "$tree" >actual &&
	test_line_count = 5 actual
	)
'

test_expect_success 'ls-tree -r does not show tree entries by default' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r "$tree" >actual &&
	! grep "^040000" actual
	)
'

test_expect_success 'ls-tree -r -t shows tree entries too' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r -t "$tree" >actual &&
	grep "^040000 tree" actual &&
	grep "^100644 blob" actual
	)
'

test_expect_success 'ls-tree -r -t shows more entries than -r alone' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r "$tree" >recursive &&
	grit ls-tree -r -t "$tree" >recursive_trees &&
	r_count=$(wc -l <recursive | tr -d " ") &&
	rt_count=$(wc -l <recursive_trees | tr -d " ") &&
	test "$rt_count" -gt "$r_count"
	)
'

###########################################################################
# Section 4: Name-only output
###########################################################################

test_expect_success 'ls-tree --name-only shows only names' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree --name-only "$tree" >actual &&
	echo "dir1" >expect &&
	echo "dir2" >>expect &&
	echo "root.txt" >>expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree -r --name-only shows recursive names' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r --name-only "$tree" >actual &&
	grep "dir1/sub1/deep.txt" actual
	)
'

test_expect_success 'ls-tree --name-only output has no OIDs' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree --name-only "$tree" >actual &&
	! grep -qE "[0-9a-f]{40}" actual
	)
'

###########################################################################
# Section 5: Trees-only mode
###########################################################################

test_expect_success 'ls-tree -d shows only tree entries' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -d "$tree" >actual &&
	test_line_count = 2 actual &&
	grep "dir1" actual &&
	grep "dir2" actual
	)
'

test_expect_success 'ls-tree -d does not show blob entries' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -d "$tree" >actual &&
	! grep "root.txt" actual
	)
'

###########################################################################
# Section 6: Long format
###########################################################################

test_expect_success 'ls-tree -l shows long format with size column' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -l "$tree" | grep "root.txt" >actual &&
	# Long format adds a size/placeholder column (tab-separated)
	test -s actual
	)
'

test_expect_success 'ls-tree -r -l shows sizes for all blobs' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r -l "$tree" >actual &&
	test_line_count = 5 actual
	)
'

###########################################################################
# Section 7: Path filtering
###########################################################################

test_expect_success 'ls-tree with path filter shows only matching entries' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree "$tree" dir1 >actual &&
	test_line_count = 1 actual &&
	grep "dir1" actual
	)
'

test_expect_success 'ls-tree -r with path filter shows nested matches' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r "$tree" dir1 >actual &&
	grep "dir1/file1.txt" actual &&
	grep "dir1/sub1/deep.txt" actual &&
	! grep "dir2" actual
	)
'

###########################################################################
# Section 8: Zero-terminated output
###########################################################################

test_expect_success 'ls-tree -z uses NUL terminators' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -z "$tree" >actual &&
	tr "\0" "\n" <actual | sed "/^$/d" >lines &&
	test_line_count = 3 lines
	)
'

test_expect_success 'ls-tree -r -z uses NUL terminators' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r -z "$tree" >actual &&
	tr "\0" "\n" <actual | sed "/^$/d" >lines &&
	test_line_count = 5 lines
	)
'

###########################################################################
# Section 9: OID verification
###########################################################################

test_expect_success 'ls-tree blob OID matches hash-object' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	blob_oid=$(grit ls-tree "$tree" | grep "root.txt" | awk "{print \$3}") &&
	expected=$(grit hash-object root.txt) &&
	test "$blob_oid" = "$expected"
	)
'

test_expect_success 'ls-tree -r nested blob OID matches hash-object' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	blob_oid=$(grit ls-tree -r "$tree" | grep "dir1/sub1/deep.txt" | awk "{print \$3}") &&
	expected=$(grit hash-object dir1/sub1/deep.txt) &&
	test "$blob_oid" = "$expected"
	)
'

test_expect_success 'ls-tree subtree OID is valid tree' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	sub_oid=$(grit ls-tree "$tree" | grep "dir1" | awk "{print \$3}") &&
	grit cat-file -t "$sub_oid" >actual &&
	echo tree >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree entries are sorted' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree --name-only "$tree" >actual &&
	sort actual >sorted &&
	test_cmp sorted actual
	)
'

test_done
