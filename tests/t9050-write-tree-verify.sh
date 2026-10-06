#!/bin/sh
# Tests for grit write-tree: tree creation from index state.

test_description='grit write-tree verification'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

EMPTY_TREE=4b825dc642cb6eb9a060e54bf8d69288fbee4904

###########################################################################
# Section 1: Basic write-tree
###########################################################################

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo
	)
'

test_expect_success 'write-tree on empty index produces empty tree OID' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	test "$tree" = "$EMPTY_TREE"
	)
'

test_expect_success 'empty tree OID is a valid tree object' '
	(
	cd repo &&
	grit cat-file -t "$EMPTY_TREE" >actual &&
	echo tree >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'empty tree has zero size' '
	(
	cd repo &&
	grit cat-file -s "$EMPTY_TREE" >actual &&
	echo 0 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'write-tree after adding one file' '
	(
	cd repo &&
	echo "hello" >hello.txt &&
	grit add hello.txt &&
	tree=$(grit write-tree) &&
	test "$tree" != "$EMPTY_TREE" &&
	grit cat-file -t "$tree" >actual &&
	echo tree >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'write-tree OID is 40 hex chars' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	echo "$tree" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'write-tree is deterministic' '
	(
	cd repo &&
	tree1=$(grit write-tree) &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

###########################################################################
# Section 2: Tree contents verification via ls-tree
###########################################################################

test_expect_success 'write-tree tree contains added file' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >listing &&
	grep "hello.txt" listing
	)
'

test_expect_success 'write-tree entry has blob type' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >listing &&
	grep "^100644 blob" listing | grep "hello.txt"
	)
'

test_expect_success 'write-tree blob OID matches hash-object OID' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	blob_oid=$(grit ls-tree "$tree" | grep "hello.txt" | awk "{print \$3}") &&
	expected_oid=$(grit hash-object hello.txt) &&
	test "$blob_oid" = "$expected_oid"
	)
'

test_expect_success 'write-tree with multiple files' '
	(
	cd repo &&
	echo "world" >world.txt &&
	echo "foo" >foo.txt &&
	grit add world.txt foo.txt &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >listing &&
	test_line_count = 3 listing
	)
'

test_expect_success 'write-tree entries are sorted alphabetically' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree --name-only "$tree" >actual &&
	sort actual >expected_sorted &&
	test_cmp expected_sorted actual
	)
'

###########################################################################
# Section 3: Subdirectories
###########################################################################

test_expect_success 'write-tree with subdirectory' '
	(
	cd repo &&
	mkdir -p subdir &&
	echo "nested" >subdir/nested.txt &&
	grit add subdir/nested.txt &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >listing &&
	grep "^040000 tree" listing | grep "subdir"
	)
'

test_expect_success 'write-tree subdirectory tree has correct contents' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	sub_tree_oid=$(grit ls-tree "$tree" | grep "subdir" | awk "{print \$3}") &&
	grit ls-tree "$sub_tree_oid" >sub_listing &&
	grep "nested.txt" sub_listing
	)
'

test_expect_success 'write-tree with deeply nested directories' '
	(
	cd repo &&
	mkdir -p a/b/c &&
	echo "deep" >a/b/c/deep.txt &&
	grit add a/b/c/deep.txt &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >listing &&
	grep "a/b/c/deep.txt" listing
	)
'

test_expect_success 'write-tree with multiple subdirectories' '
	(
	cd repo &&
	mkdir -p dir1 dir2 &&
	echo "one" >dir1/one.txt &&
	echo "two" >dir2/two.txt &&
	grit add dir1/one.txt dir2/two.txt &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" | grep "^040000 tree" >trees &&
	grep "dir1" trees &&
	grep "dir2" trees
	)
'

###########################################################################
# Section 4: Index modifications
###########################################################################

test_expect_success 'write-tree changes after modifying a file' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	echo "modified" >hello.txt &&
	grit add hello.txt &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after"
	)
'

test_expect_success 'write-tree changes after removing a file' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	grit rm -f foo.txt &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after"
	)
'

test_expect_success 'write-tree after rm shows fewer entries' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" --name-only >listing &&
	! grep "foo.txt" listing
	)
'

test_expect_success 'write-tree after re-adding same content gives same tree' '
	(
	cd repo &&
	echo "stable" >stable.txt &&
	grit add stable.txt &&
	tree1=$(grit write-tree) &&
	echo "changed" >stable.txt &&
	grit add stable.txt &&
	echo "stable" >stable.txt &&
	grit add stable.txt &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

###########################################################################
# Section 5: Prefix option
###########################################################################

test_expect_success 'write-tree with file in root and subdirectory' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >listing &&
	grep "hello.txt" listing &&
	grep "subdir" listing
	)
'

test_expect_success 'write-tree after adding symlink-like paths' '
	(
	cd repo &&
	mkdir -p linkdir &&
	echo "target" >linkdir/target.txt &&
	grit add linkdir/target.txt &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >listing &&
	grep "linkdir/target.txt" listing
	)
'

###########################################################################
# Section 6: Edge cases
###########################################################################

test_expect_success 'write-tree output is stored in ODB' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit cat-file -e "$tree"
	)
'

test_expect_success 'write-tree tree object can be read back' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit cat-file -p "$tree" >output &&
	test -s output
	)
'

test_expect_success 'write-tree with executable file' '
	(
	cd repo &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	grit add script.sh &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" | grep "script.sh" >entry &&
	grep "^100755" entry
	)
'

test_expect_success 'fresh repo write-tree equals empty tree' '
	(
	grit init fresh_repo &&
	cd fresh_repo &&
	tree=$(grit write-tree) &&
	test "$tree" = "$EMPTY_TREE"
	)
'

test_done
