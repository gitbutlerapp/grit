#!/bin/sh
# Test mktree with nested directory structures, various entry formats,
# sorting, and round-trip through ls-tree.

test_description='grit mktree with nested directories'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test"
	)
'

test_expect_success 'create blob objects for testing' '
	(
	cd repo &&
	echo "alpha content" | grit hash-object -w --stdin >blob_alpha &&
	echo "beta content" | grit hash-object -w --stdin >blob_beta &&
	echo "gamma content" | grit hash-object -w --stdin >blob_gamma &&
	echo "delta content" | grit hash-object -w --stdin >blob_delta &&
	printf "" | grit hash-object -w --stdin >blob_empty
	)
'

###########################################################################
# Section 2: Basic mktree
###########################################################################

test_expect_success 'mktree with single blob entry' '
	(
	cd repo &&
	blob=$(cat blob_alpha) &&
	printf "100644 blob %s\ta.txt\n" "$blob" | grit mktree >tree_oid &&
	test -s tree_oid
	)
'

test_expect_success 'mktree output is valid tree OID' '
	(
	cd repo &&
	oid=$(cat tree_oid) &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'mktree tree is inspectable with cat-file' '
	(
	cd repo &&
	oid=$(cat tree_oid) &&
	grit cat-file -t "$oid" >type &&
	echo "tree" >expect &&
	test_cmp expect type
	)
'

test_expect_success 'mktree single entry round-trips through ls-tree' '
	(
	cd repo &&
	blob=$(cat blob_alpha) &&
	oid=$(cat tree_oid) &&
	grit ls-tree "$oid" >actual &&
	grep "a.txt" actual &&
	grep "$blob" actual
	)
'

###########################################################################
# Section 3: Multiple entries
###########################################################################

test_expect_success 'mktree with two blob entries' '
	(
	cd repo &&
	ba=$(cat blob_alpha) &&
	bb=$(cat blob_beta) &&
	printf "100644 blob %s\ta.txt\n100644 blob %s\tb.txt\n" "$ba" "$bb" |
		grit mktree >tree2_oid &&
	test -s tree2_oid
	)
'

test_expect_success 'mktree with three entries' '
	(
	cd repo &&
	ba=$(cat blob_alpha) &&
	bb=$(cat blob_beta) &&
	bg=$(cat blob_gamma) &&
	printf "100644 blob %s\ta.txt\n100644 blob %s\tb.txt\n100644 blob %s\tc.txt\n" \
		"$ba" "$bb" "$bg" | grit mktree >tree3_oid &&
	oid=$(cat tree3_oid) &&
	grit ls-tree "$oid" >out &&
	test_line_count = 3 out
	)
'

test_expect_success 'mktree entries appear in ls-tree output' '
	(
	cd repo &&
	oid=$(cat tree3_oid) &&
	grit ls-tree "$oid" >out &&
	grep "a.txt" out &&
	grep "b.txt" out &&
	grep "c.txt" out
	)
'

###########################################################################
# Section 4: Nested trees (subdirectories)
###########################################################################

test_expect_success 'create inner tree for subdirectory' '
	(
	cd repo &&
	bg=$(cat blob_gamma) &&
	bd=$(cat blob_delta) &&
	printf "100644 blob %s\tg.txt\n100644 blob %s\td.txt\n" "$bg" "$bd" |
		grit mktree >inner_tree_oid &&
	test -s inner_tree_oid
	)
'

test_expect_success 'mktree with nested tree entry' '
	(
	cd repo &&
	ba=$(cat blob_alpha) &&
	inner=$(cat inner_tree_oid) &&
	printf "100644 blob %s\ta.txt\n040000 tree %s\tsub\n" "$ba" "$inner" |
		grit mktree >nested_tree_oid &&
	test -s nested_tree_oid
	)
'

test_expect_success 'nested tree shows sub as tree in ls-tree' '
	(
	cd repo &&
	oid=$(cat nested_tree_oid) &&
	grit ls-tree "$oid" >out &&
	grep "040000" out | grep "tree" | grep "sub"
	)
'

test_expect_success 'ls-tree on inner tree shows files' '
	(
	cd repo &&
	inner=$(cat inner_tree_oid) &&
	grit ls-tree "$inner" >out &&
	grep "g.txt" out &&
	grep "d.txt" out
	)
'

test_expect_success 'nested tree cat-file -p matches git' '
	(
	cd repo &&
	oid=$(cat nested_tree_oid) &&
	grit cat-file -p "$oid" >grit_out &&
	git cat-file -p "$oid" >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 5: Deeply nested (3 levels)
###########################################################################

test_expect_success 'create deeply nested tree structure' '
	(
	cd repo &&
	bd=$(cat blob_delta) &&
	printf "100644 blob %s\tleaf.txt\n" "$bd" | grit mktree >deep_leaf &&
	leaf_tree=$(cat deep_leaf) &&
	printf "040000 tree %s\tdeep\n" "$leaf_tree" | grit mktree >deep_mid &&
	mid_tree=$(cat deep_mid) &&
	ba=$(cat blob_alpha) &&
	printf "100644 blob %s\troot.txt\n040000 tree %s\tmid\n" "$ba" "$mid_tree" |
		grit mktree >deep_root &&
	test -s deep_root
	)
'

test_expect_success 'deep tree root has 2 entries' '
	(
	cd repo &&
	oid=$(cat deep_root) &&
	grit ls-tree "$oid" >out &&
	test_line_count = 2 out
	)
'

test_expect_success 'deep tree mid level has 1 entry' '
	(
	cd repo &&
	oid=$(cat deep_root) &&
	mid_oid=$(grit ls-tree "$oid" | grep "mid" | awk "{print \$3}") &&
	grit ls-tree "$mid_oid" >out &&
	test_line_count = 1 out
	)
'

test_expect_success 'deep tree leaf level has 1 entry' '
	(
	cd repo &&
	oid=$(cat deep_root) &&
	mid_oid=$(grit ls-tree "$oid" | grep "mid" | awk "{print \$3}") &&
	deep_oid=$(grit ls-tree "$mid_oid" | grep "deep" | awk "{print \$3}") &&
	grit ls-tree "$deep_oid" >out &&
	test_line_count = 1 out &&
	grep "leaf.txt" out
	)
'

###########################################################################
# Section 6: mktree matches git mktree
###########################################################################

test_expect_success 'mktree output matches git mktree for single entry' '
	(
	cd repo &&
	ba=$(cat blob_alpha) &&
	input=$(printf "100644 blob %s\ta.txt\n" "$ba") &&
	grit_oid=$(echo "$input" | grit mktree) &&
	git_oid=$(echo "$input" | git mktree) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'mktree output matches git mktree for multiple entries' '
	(
	cd repo &&
	ba=$(cat blob_alpha) &&
	bb=$(cat blob_beta) &&
	input=$(printf "100644 blob %s\ta.txt\n100644 blob %s\tb.txt\n" "$ba" "$bb") &&
	grit_oid=$(echo "$input" | grit mktree) &&
	git_oid=$(echo "$input" | git mktree) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'mktree with tree entry matches git mktree' '
	(
	cd repo &&
	ba=$(cat blob_alpha) &&
	inner=$(cat inner_tree_oid) &&
	input=$(printf "100644 blob %s\ta.txt\n040000 tree %s\tsub\n" "$ba" "$inner") &&
	grit_oid=$(echo "$input" | grit mktree) &&
	git_oid=$(echo "$input" | git mktree) &&
	test "$grit_oid" = "$git_oid"
	)
'

###########################################################################
# Section 7: Empty tree
###########################################################################

test_expect_success 'mktree with empty input creates empty tree' '
	(
	cd repo &&
	oid=$(printf "" | grit mktree) &&
	test -n "$oid"
	)
'

test_expect_success 'empty tree has well-known SHA' '
	(
	cd repo &&
	oid=$(printf "" | grit mktree) &&
	test "$oid" = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
	)
'

test_expect_success 'empty tree matches git empty tree' '
	(
	cd repo &&
	grit_oid=$(printf "" | grit mktree) &&
	git_oid=$(printf "" | git mktree) &&
	test "$grit_oid" = "$git_oid"
	)
'

###########################################################################
# Section 8: Executable and symlink modes
###########################################################################

test_expect_success 'mktree with executable mode 100755' '
	(
	cd repo &&
	ba=$(cat blob_alpha) &&
	printf "100755 blob %s\tscript.sh\n" "$ba" | grit mktree >exec_tree &&
	oid=$(cat exec_tree) &&
	grit ls-tree "$oid" >out &&
	grep "100755" out
	)
'

test_expect_success 'mktree with symlink mode 120000' '
	(
	cd repo &&
	link_target="target_path" &&
	link_oid=$(printf "%s" "$link_target" | grit hash-object -w --stdin) &&
	printf "120000 blob %s\tmy_link\n" "$link_oid" | grit mktree >link_tree &&
	oid=$(cat link_tree) &&
	grit ls-tree "$oid" >out &&
	grep "120000" out
	)
'

test_expect_success 'mktree mixed modes' '
	(
	cd repo &&
	ba=$(cat blob_alpha) &&
	bb=$(cat blob_beta) &&
	printf "100644 blob %s\tnormal.txt\n100755 blob %s\texec.sh\n" "$ba" "$bb" |
		grit mktree >mixed_tree &&
	oid=$(cat mixed_tree) &&
	grit ls-tree "$oid" >out &&
	grep "100644" out &&
	grep "100755" out
	)
'

###########################################################################
# Section 9: Sorting behavior
###########################################################################

test_expect_success 'mktree with entries in reverse order matches sorted' '
	(
	cd repo &&
	ba=$(cat blob_alpha) &&
	bb=$(cat blob_beta) &&
	input_sorted=$(printf "100644 blob %s\taaa\n100644 blob %s\tzzz\n" "$ba" "$bb") &&
	input_reverse=$(printf "100644 blob %s\tzzz\n100644 blob %s\taaa\n" "$bb" "$ba") &&
	oid_sorted=$(echo "$input_sorted" | grit mktree) &&
	oid_reverse=$(echo "$input_reverse" | grit mktree) &&
	test "$oid_sorted" = "$oid_reverse"
	)
'

###########################################################################
# Section 10: Edge cases
###########################################################################

test_expect_success 'mktree with empty blob' '
	(
	cd repo &&
	be=$(cat blob_empty) &&
	printf "100644 blob %s\tempty.txt\n" "$be" | grit mktree >empty_blob_tree &&
	oid=$(cat empty_blob_tree) &&
	grit cat-file -t "$oid" >type &&
	echo "tree" >expect &&
	test_cmp expect type
	)
'

test_expect_success 'mktree with dot-prefixed filename' '
	(
	cd repo &&
	ba=$(cat blob_alpha) &&
	printf "100644 blob %s\t.hidden\n" "$ba" | grit mktree >dot_tree &&
	oid=$(cat dot_tree) &&
	grit ls-tree "$oid" >out &&
	grep ".hidden" out
	)
'

test_expect_success 'mktree with filename containing spaces' '
	(
	cd repo &&
	ba=$(cat blob_alpha) &&
	printf "100644 blob %s\tmy file.txt\n" "$ba" | grit mktree >space_tree &&
	oid=$(cat space_tree) &&
	grit ls-tree "$oid" >out &&
	grep "my file.txt" out
	)
'

test_expect_success 'large tree with many entries' '
	(
	cd repo &&
	ba=$(cat blob_alpha) &&
	for i in $(seq -w 1 50); do
		printf "100644 blob %s\tfile_%s.txt\n" "$ba" "$i"
	done | grit mktree >big_tree &&
	oid=$(cat big_tree) &&
	grit ls-tree "$oid" >out &&
	test_line_count = 50 out
	)
'

test_done
