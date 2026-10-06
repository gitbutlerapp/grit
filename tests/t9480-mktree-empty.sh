#!/bin/sh
# Tests for grit mktree: building tree objects from ls-tree formatted input,
# including empty trees, -z mode, --missing, and --batch.

test_description='grit mktree empty trees and batch mode'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git
EMPTY_TREE=4b825dc642cb6eb9a060e54bf8d69288fbee4904

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	echo "hello" >hello.txt &&
	grit add hello.txt &&
	blob_oid=$(grit hash-object -w hello.txt) &&
	echo "$blob_oid" >../blob_oid &&
	tree_oid=$(grit write-tree) &&
	echo "$tree_oid" >../tree_oid
	)
'

###########################################################################
# Section 2: Empty tree
###########################################################################

test_expect_success 'mktree with empty input creates empty tree' '
	(
	cd repo &&
	oid=$(printf "" | grit mktree) &&
	test "$oid" = "$EMPTY_TREE"
	)
'

test_expect_success 'empty tree from mktree matches well-known SHA1' '
	(
	cd repo &&
	oid=$(printf "" | grit mktree) &&
	test "$oid" = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
	)
'

test_expect_success 'empty tree object exists in ODB after mktree' '
	(
	cd repo &&
	printf "" | grit mktree >/dev/null &&
	grit cat-file -e "$EMPTY_TREE"
	)
'

test_expect_success 'empty tree has type tree' '
	(
	cd repo &&
	printf "" | grit mktree >/dev/null &&
	grit cat-file -t "$EMPTY_TREE" >actual &&
	echo tree >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'empty tree has size 0' '
	(
	cd repo &&
	printf "" | grit mktree >/dev/null &&
	grit cat-file -s "$EMPTY_TREE" >actual &&
	echo 0 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree on empty tree produces no output' '
	(
	cd repo &&
	printf "" | grit mktree >/dev/null &&
	grit ls-tree "$EMPTY_TREE" >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 3: Single-entry trees
###########################################################################

test_expect_success 'mktree with one blob entry' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "100644 blob $blob	hello.txt" | grit mktree >actual_oid &&
	test -s actual_oid
	)
'

test_expect_success 'mktree single entry matches write-tree' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	tree_from_mktree=$(echo "100644 blob $blob	hello.txt" | grit mktree) &&
	tree_from_write=$(cat ../tree_oid) &&
	test "$tree_from_mktree" = "$tree_from_write"
	)
'

test_expect_success 'mktree created tree is readable via ls-tree' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	tree=$(echo "100644 blob $blob	hello.txt" | grit mktree) &&
	grit ls-tree "$tree" >actual &&
	grep "hello.txt" actual
	)
'

test_expect_success 'mktree entry has correct mode in ls-tree output' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	tree=$(echo "100644 blob $blob	hello.txt" | grit mktree) &&
	grit ls-tree "$tree" | grep "hello.txt" >actual &&
	grep "^100644 blob" actual
	)
'

###########################################################################
# Section 4: Multi-entry trees
###########################################################################

test_expect_success 'mktree with multiple blob entries' '
	(
	cd repo &&
	echo "file a" >a.txt &&
	echo "file b" >b.txt &&
	oid_a=$(grit hash-object -w a.txt) &&
	oid_b=$(grit hash-object -w b.txt) &&
	printf "100644 blob %s\ta.txt\n100644 blob %s\tb.txt\n" "$oid_a" "$oid_b" | grit mktree >tree_oid &&
	test -s tree_oid
	)
'

test_expect_success 'mktree multi-entry tree lists both entries' '
	(
	cd repo &&
	tree=$(cat tree_oid) &&
	grit ls-tree "$tree" >actual &&
	grep "a.txt" actual &&
	grep "b.txt" actual
	)
'

test_expect_success 'mktree with nested tree entry' '
	(
	cd repo &&
	echo "inner" >inner.txt &&
	inner_blob=$(grit hash-object -w inner.txt) &&
	sub_tree=$(echo "100644 blob $inner_blob	inner.txt" | grit mktree) &&
	blob=$(cat ../blob_oid) &&
	printf "100644 blob %s\troot.txt\n040000 tree %s\tsubdir\n" "$blob" "$sub_tree" | grit mktree >nested_tree &&
	test -s nested_tree
	)
'

test_expect_success 'mktree nested tree ls-tree shows subdir' '
	(
	cd repo &&
	tree=$(cat nested_tree) &&
	grit ls-tree "$tree" >actual &&
	grep "subdir" actual &&
	grep "^040000 tree" actual
	)
'

test_expect_success 'mktree nested tree recursive ls-tree shows inner file' '
	(
	cd repo &&
	tree=$(cat nested_tree) &&
	grit ls-tree -r "$tree" >actual &&
	grep "subdir/inner.txt" actual
	)
'

###########################################################################
# Section 5: -z (NUL-terminated) mode
###########################################################################

test_expect_success 'mktree -z with NUL-terminated input' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	printf "100644 blob %s\thello.txt\0" "$blob" | grit mktree -z >z_tree &&
	test -s z_tree
	)
'

test_expect_success 'mktree -z produces same tree as newline mode' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	tree_nl=$(echo "100644 blob $blob	hello.txt" | grit mktree) &&
	tree_z=$(printf "100644 blob %s\thello.txt\0" "$blob" | grit mktree -z) &&
	test "$tree_nl" = "$tree_z"
	)
'

test_expect_success 'mktree -z with multiple entries' '
	(
	cd repo &&
	echo "za" >za.txt &&
	echo "zb" >zb.txt &&
	oid_za=$(grit hash-object -w za.txt) &&
	oid_zb=$(grit hash-object -w zb.txt) &&
	printf "100644 blob %s\tza.txt\0100644 blob %s\tzb.txt\0" "$oid_za" "$oid_zb" | grit mktree -z >z_multi &&
	tree=$(cat z_multi) &&
	grit ls-tree "$tree" >actual &&
	grep "za.txt" actual &&
	grep "zb.txt" actual
	)
'

###########################################################################
# Section 6: --batch mode
###########################################################################

test_expect_success 'mktree --batch creates multiple trees' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "x" >x.txt &&
	oid_x=$(grit hash-object -w x.txt) &&
	printf "100644 blob %s\thello.txt\n\n100644 blob %s\tx.txt\n" "$blob" "$oid_x" | grit mktree --batch >batch_trees &&
	test $(wc -l <batch_trees) -eq 2
	)
'

test_expect_success 'mktree --batch first tree is correct' '
	(
	cd repo &&
	tree1=$(sed -n 1p batch_trees) &&
	grit ls-tree "$tree1" >actual &&
	grep "hello.txt" actual
	)
'

test_expect_success 'mktree --batch second tree is correct' '
	(
	cd repo &&
	tree2=$(sed -n 2p batch_trees) &&
	grit ls-tree "$tree2" >actual &&
	grep "x.txt" actual
	)
'

test_expect_success 'mktree --batch empty input produces empty tree' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	printf "\n100644 blob %s\thello.txt\n" "$blob" | grit mktree --batch >batch_empty &&
	tree1=$(sed -n 1p batch_empty) &&
	test "$tree1" = "$EMPTY_TREE"
	)
'

###########################################################################
# Section 7: --missing flag
###########################################################################

test_expect_success 'mktree --missing allows non-existent object' '
	(
	cd repo &&
	fake_oid="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" &&
	echo "100644 blob $fake_oid	phantom.txt" | grit mktree --missing >missing_tree &&
	test -s missing_tree
	)
'

###########################################################################
# Section 8: Cross-check with real git
###########################################################################

test_expect_success 'mktree output matches real git for single blob' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	tree_grit=$(echo "100644 blob $blob	hello.txt" | grit mktree) &&
	tree_git=$(echo "100644 blob $blob	hello.txt" | $REAL_GIT mktree) &&
	test "$tree_grit" = "$tree_git"
	)
'

test_expect_success 'mktree empty tree matches real git' '
	(
	cd repo &&
	tree_grit=$(printf "" | grit mktree) &&
	tree_git=$(printf "" | $REAL_GIT mktree) &&
	test "$tree_grit" = "$tree_git"
	)
'

test_expect_success 'mktree deterministic: same input gives same OID' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	printf "100644 blob %s\thello.txt\n" "$blob" >mktree_input &&
	tree1=$(grit mktree <mktree_input) &&
	tree2=$(grit mktree <mktree_input) &&
	test "$tree1" = "$tree2"
	)
'

test_done
