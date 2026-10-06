#!/bin/sh
# Tests for grit write-tree with nested directory structures.

test_description='grit write-tree with nested directories'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repository' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com"
	)
'

###########################################################################
# Section 2: Basic write-tree
###########################################################################

test_expect_success 'write-tree on empty index produces empty tree' '
	(
	cd repo &&
	tree_oid=$(grit write-tree) &&
	grit cat-file -t "$tree_oid" >actual &&
	echo tree >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'write-tree empty tree matches real git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

test_expect_success 'write-tree with single file' '
	(
	cd repo &&
	echo "hello" >file.txt &&
	grit update-index --add file.txt &&
	tree_oid=$(grit write-tree) &&
	grit ls-tree "$tree_oid" >actual &&
	grep "file.txt" actual
	)
'

test_expect_success 'write-tree single file matches real git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

###########################################################################
# Section 3: Single-level nesting
###########################################################################

test_expect_success 'write-tree with one subdirectory' '
	(
	cd repo &&
	mkdir -p sub &&
	echo "nested" >sub/file.txt &&
	grit update-index --add sub/file.txt &&
	tree_oid=$(grit write-tree) &&
	grit ls-tree "$tree_oid" >actual &&
	grep "040000 tree" actual | grep "sub"
	)
'

test_expect_success 'write-tree subdirectory matches real git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

test_expect_success 'write-tree subtree has correct blob' '
	(
	cd repo &&
	tree_oid=$(grit write-tree) &&
	sub_tree=$(grit ls-tree "$tree_oid" -- sub | awk "{print \$3}") &&
	grit ls-tree "$sub_tree" >actual &&
	grep "file.txt" actual
	)
'

test_expect_success 'write-tree subtree blob content is correct' '
	(
	cd repo &&
	tree_oid=$(grit write-tree) &&
	blob_oid=$(grit ls-tree -r "$tree_oid" | grep "sub/file.txt" | awk "{print \$3}") &&
	grit cat-file -p "$blob_oid" >actual &&
	echo "nested" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 4: Deep nesting
###########################################################################

test_expect_success 'write-tree with two-level nesting' '
	(
	cd repo &&
	mkdir -p a/b &&
	echo "deep" >a/b/deep.txt &&
	grit update-index --add a/b/deep.txt &&
	tree_oid=$(grit write-tree) &&
	grit ls-tree -r "$tree_oid" >actual &&
	grep "a/b/deep.txt" actual
	)
'

test_expect_success 'write-tree two-level nesting matches real git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

test_expect_success 'write-tree with three-level nesting' '
	(
	cd repo &&
	mkdir -p x/y/z &&
	echo "very deep" >x/y/z/leaf.txt &&
	grit update-index --add x/y/z/leaf.txt &&
	tree_oid=$(grit write-tree) &&
	grit ls-tree -r "$tree_oid" >actual &&
	grep "x/y/z/leaf.txt" actual
	)
'

test_expect_success 'write-tree three-level nesting matches real git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

test_expect_success 'write-tree recursive output shows all nested files' '
	(
	cd repo &&
	tree_oid=$(grit write-tree) &&
	grit ls-tree -r "$tree_oid" >actual &&
	grep "file.txt" actual &&
	grep "sub/file.txt" actual &&
	grep "a/b/deep.txt" actual &&
	grep "x/y/z/leaf.txt" actual
	)
'

###########################################################################
# Section 5: Multiple files in nested directories
###########################################################################

test_expect_success 'write-tree multiple files in same subdirectory' '
	(
	cd repo &&
	echo "one" >sub/one.txt &&
	echo "two" >sub/two.txt &&
	grit update-index --add sub/one.txt sub/two.txt &&
	tree_oid=$(grit write-tree) &&
	sub_tree=$(grit ls-tree "$tree_oid" -- sub | awk "{print \$3}") &&
	grit ls-tree "$sub_tree" >actual &&
	test $(wc -l <actual) -ge 3
	)
'

test_expect_success 'write-tree multiple nested files matches real git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

###########################################################################
# Section 6: Sibling directories
###########################################################################

test_expect_success 'write-tree with sibling directories' '
	(
	cd repo &&
	mkdir -p dir1 dir2 &&
	echo "in dir1" >dir1/f.txt &&
	echo "in dir2" >dir2/f.txt &&
	grit update-index --add dir1/f.txt dir2/f.txt &&
	tree_oid=$(grit write-tree) &&
	grit ls-tree "$tree_oid" >actual &&
	grep "dir1" actual &&
	grep "dir2" actual
	)
'

test_expect_success 'write-tree sibling directories matches real git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

###########################################################################
# Section 7: Mixed files and directories at same level
###########################################################################

test_expect_success 'write-tree files and dirs at root level' '
	(
	cd repo &&
	tree_oid=$(grit write-tree) &&
	grit ls-tree "$tree_oid" >actual &&
	grep "^100644 blob" actual &&
	grep "^040000 tree" actual
	)
'

test_expect_success 'write-tree root entries are sorted' '
	(
	cd repo &&
	tree_oid=$(grit write-tree) &&
	grit ls-tree --name-only "$tree_oid" >actual &&
	sort actual >sorted &&
	test_cmp sorted actual
	)
'

###########################################################################
# Section 8: write-tree round-trip with mktree
###########################################################################

test_expect_success 'write-tree output can be reconstructed via mktree' '
	(
	cd repo &&
	tree_oid=$(grit write-tree) &&
	grit ls-tree "$tree_oid" | grit mktree >reconstructed &&
	echo "$tree_oid" >original &&
	test_cmp original reconstructed
	)
'

test_expect_success 'write-tree recursive listing matches real git' '
	(
	cd repo &&
	tree_oid=$(grit write-tree) &&
	grit ls-tree -r "$tree_oid" >actual &&
	"$REAL_GIT" ls-tree -r "$tree_oid" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 9: Index modifications and write-tree
###########################################################################

test_expect_success 'write-tree after modifying a nested file' '
	(
	cd repo &&
	echo "modified" >sub/file.txt &&
	grit update-index sub/file.txt &&
	new_tree=$(grit write-tree) &&
	old_tree=$("$REAL_GIT" rev-parse HEAD^{tree} 2>/dev/null || echo "none") &&
	test "$new_tree" != "$old_tree" || test "$old_tree" = "none"
	)
'

test_expect_success 'write-tree after modification matches real git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

###########################################################################
# Section 10: write-tree with executable in nested dir
###########################################################################

test_expect_success 'write-tree preserves executable mode in nested dir' '
	(
	cd repo &&
	mkdir -p bin &&
	echo "#!/bin/sh" >bin/run.sh &&
	chmod +x bin/run.sh &&
	grit update-index --add bin/run.sh &&
	tree_oid=$(grit write-tree) &&
	grit ls-tree -r "$tree_oid" | grep "bin/run.sh" >actual &&
	grep "^100755" actual
	)
'

test_expect_success 'write-tree executable nested matches real git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

###########################################################################
# Section 11: write-tree idempotency
###########################################################################

test_expect_success 'write-tree is idempotent (same index = same tree)' '
	(
	cd repo &&
	tree1=$(grit write-tree) &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'write-tree produces valid tree verifiable by cat-file' '
	(
	cd repo &&
	tree_oid=$(grit write-tree) &&
	grit cat-file -t "$tree_oid" >actual &&
	echo tree >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'write-tree OID is 40 hex chars' '
	(
	cd repo &&
	tree_oid=$(grit write-tree) &&
	echo "$tree_oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_done
