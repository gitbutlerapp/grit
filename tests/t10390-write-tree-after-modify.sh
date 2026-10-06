#!/bin/sh
# Test write-tree behavior after various index modifications: add, remove,
# modify, rename, chmod, subdirectory changes, and interaction with
# update-index and read-tree.

test_description='grit write-tree after index modifications'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

EMPTY_TREE='4b825dc642cb6eb9a060e54bf8d69288fbee4904'

test_expect_success 'setup repo' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "original" >file1 &&
	echo "second" >file2 &&
	echo "third" >file3 &&
	grit update-index --add file1 file2 file3 &&
	tree_base=$(grit write-tree) &&
	echo "$tree_base" >../tree_base
	)
'

test_expect_success 'write-tree after adding one file changes tree' '
	(
	cd repo &&
	echo "new" >file4 &&
	grit update-index --add file4 &&
	tree=$(grit write-tree) &&
	test "$tree" != "$(cat ../tree_base)" &&
	echo "$tree" >../tree_4files
	)
'

test_expect_success 'write-tree after adding is idempotent' '
	(
	cd repo &&
	tree1=$(grit write-tree) &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'write-tree after removing file changes tree' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	grit update-index --force-remove file4 &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after"
	)
'

test_expect_success 'write-tree after remove matches original 3-file tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	test "$tree" = "$(cat ../tree_base)"
	)
'

test_expect_success 'write-tree after modifying file content' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	echo "modified content" >file1 &&
	grit update-index --add file1 &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after" &&
	echo "$tree_after" >../tree_modified
	)
'

test_expect_success 'write-tree after restoring content matches original' '
	(
	cd repo &&
	echo "original" >file1 &&
	grit update-index --add file1 &&
	tree=$(grit write-tree) &&
	test "$tree" = "$(cat ../tree_base)"
	)
'

test_expect_success 'write-tree: add and remove different files' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	echo "new_content" >new_file &&
	grit update-index --add new_file &&
	grit update-index --force-remove file3 &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after"
	)
'

test_expect_success 'write-tree: restore both changes returns to base' '
	(
	cd repo &&
	echo "third" >file3 &&
	grit update-index --add file3 &&
	grit update-index --force-remove new_file &&
	tree=$(grit write-tree) &&
	test "$tree" = "$(cat ../tree_base)"
	)
'

test_expect_success 'write-tree with subdirectory' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	mkdir -p sub &&
	echo "nested" >sub/inner &&
	grit update-index --add sub/inner &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after" &&
	echo "$tree_after" >../tree_sub
	)
'

test_expect_success 'write-tree subdirectory appears as tree entry' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "040000 tree.*sub" actual
	)
'

test_expect_success 'write-tree: nested file appears in recursive ls-tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >actual &&
	grep "sub/inner" actual
	)
'

test_expect_success 'write-tree: modify nested file changes tree' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	echo "modified nested" >sub/inner &&
	grit update-index --add sub/inner &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after"
	)
'

test_expect_success 'write-tree: restore nested file restores tree' '
	(
	cd repo &&
	echo "nested" >sub/inner &&
	grit update-index --add sub/inner &&
	tree=$(grit write-tree) &&
	test "$tree" = "$(cat ../tree_sub)"
	)
'

test_expect_success 'write-tree: deeply nested directory' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	mkdir -p a/b/c &&
	echo "deep" >a/b/c/deep &&
	grit update-index --add a/b/c/deep &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after" &&
	grit ls-tree -r "$tree_after" >actual &&
	grep "a/b/c/deep" actual
	)
'

test_expect_success 'write-tree: remove deeply nested file' '
	(
	cd repo &&
	grit update-index --force-remove a/b/c/deep &&
	tree=$(grit write-tree) &&
	test "$tree" = "$(cat ../tree_sub)"
	)
'

test_expect_success 'write-tree: executable mode change' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	chmod +x file1 &&
	grit update-index --add file1 &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after" &&
	grit ls-tree "$tree_after" >actual &&
	grep "100755.*file1" actual
	)
'

test_expect_success 'write-tree: restore normal mode' '
	(
	cd repo &&
	chmod -x file1 &&
	grit update-index --add file1 &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "100644.*file1" actual
	)
'

test_expect_success 'write-tree: multiple files same content different names' '
	(
	cd repo &&
	echo "shared content" >dup1 &&
	echo "shared content" >dup2 &&
	echo "shared content" >dup3 &&
	grit update-index --add dup1 dup2 dup3 &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "dup1" actual &&
	grep "dup2" actual &&
	grep "dup3" actual
	)
'

test_expect_success 'write-tree: duplicate content blobs share same OID in ls-tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	oid1=$(grep "dup1" actual | awk "{print \$3}") &&
	oid2=$(grep "dup2" actual | awk "{print \$3}") &&
	oid3=$(grep "dup3" actual | awk "{print \$3}") &&
	test "$oid1" = "$oid2" &&
	test "$oid2" = "$oid3"
	)
'

test_expect_success 'write-tree: remove all duplicates' '
	(
	cd repo &&
	grit update-index --force-remove dup1 &&
	grit update-index --force-remove dup2 &&
	grit update-index --force-remove dup3 &&
	grit write-tree >actual
	)
'

test_expect_success 'write-tree: many files at once' '
	(
	cd repo &&
	for i in $(seq 1 15); do
		echo "batch content $i" >"batch_$i" &&
		grit update-index --add "batch_$i" || return 1
	done &&
	tree=$(grit write-tree) &&
	grit cat-file -t "$tree" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'write-tree: many files idempotent' '
	(
	cd repo &&
	tree1=$(grit write-tree) &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'write-tree: remove all batch files' '
	(
	cd repo &&
	for i in $(seq 1 15); do
		grit update-index --force-remove "batch_$i" || return 1
	done &&
	grit write-tree >actual
	)
'

test_expect_success 'write-tree: matches mktree from its own ls-tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" | grit mktree >from_mktree &&
	echo "$tree" >expect &&
	test_cmp expect from_mktree
	)
'

test_expect_success 'write-tree: update-index --refresh does not change tree' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	grit update-index --refresh &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" = "$tree_after"
	)
'

test_expect_success 'write-tree: read-tree then write-tree round-trips' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	rm -f .git/index &&
	grit read-tree "$tree_before" &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" = "$tree_after"
	)
'

test_expect_success 'write-tree: empty file in index' '
	(
	cd repo &&
	printf "" >empty_file &&
	grit update-index --add empty_file &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "empty_file" actual &&
	blob_oid=$(grep "empty_file" actual | awk "{print \$3}") &&
	size=$(grit cat-file -s "$blob_oid") &&
	test "$size" = "0"
	)
'

test_expect_success 'write-tree: replace file with different content' '
	(
	cd repo &&
	echo "version1" >versioned &&
	grit update-index --add versioned &&
	tree1=$(grit write-tree) &&
	echo "version2" >versioned &&
	grit update-index --add versioned &&
	tree2=$(grit write-tree) &&
	test "$tree1" != "$tree2"
	)
'

test_expect_success 'write-tree: cat-file -s returns positive for non-empty tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	size=$(grit cat-file -s "$tree") &&
	test "$size" -gt 0
	)
'

test_expect_success 'write-tree: clear entire index gives empty tree' '
	(
	cd repo &&
	grit ls-files --cached | while read f; do
		grit update-index --force-remove "$f" || return 1
	done &&
	tree=$(grit write-tree) &&
	test "$tree" = "$EMPTY_TREE"
	)
'

test_expect_success 'write-tree: rebuild index from scratch same as before' '
	(
	cd repo &&
	echo "original" >file1 &&
	echo "second" >file2 &&
	echo "third" >file3 &&
	grit update-index --add file1 file2 file3 &&
	tree=$(grit write-tree) &&
	test "$tree" = "$(cat ../tree_base)"
	)
'

test_done
