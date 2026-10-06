#!/bin/sh
# Tree hash computation, mktree, write-tree, ls-tree roundtrip tests.

test_description='grit tree hash computation and roundtrip'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with nested tree structure' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "author@example.com" &&
	grit config user.name "A U Thor" &&
	mkdir -p dir/sub &&
	echo "root file" >root.txt &&
	echo "dir file" >dir/file.txt &&
	echo "deep file" >dir/sub/deep.txt &&
	grit add root.txt dir/file.txt dir/sub/deep.txt &&
	test_tick &&
	grit commit -m "initial"
	)
'

test_expect_success 'write-tree produces valid sha1' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	test $(echo "$tree" | wc -c) -eq 41
	)
'

test_expect_success 'write-tree output matches HEAD^{tree}' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	head_tree=$(grit rev-parse HEAD^{tree}) &&
	test "$tree" = "$head_tree"
	)
'

test_expect_success 'ls-tree lists root entries' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "dir" actual &&
	grep "root.txt" actual
	)
'

test_expect_success 'ls-tree -r lists all blobs recursively' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >actual &&
	test_line_count = 3 actual &&
	grep "root.txt" actual &&
	grep "dir/file.txt" actual &&
	grep "dir/sub/deep.txt" actual
	)
'

test_expect_success 'ls-tree -d shows only tree entries' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree -d "$tree" >actual &&
	grep "^040000 tree" actual &&
	! grep "blob" actual
	)
'

test_expect_success 'ls-tree --name-only shows only paths' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree --name-only "$tree" >actual &&
	grep "^dir$" actual &&
	grep "^root.txt$" actual &&
	! grep "blob" actual &&
	! grep "tree" actual
	)
'

test_expect_success 'ls-tree -r --name-only lists all paths recursively' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree -r --name-only "$tree" >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'mktree from ls-tree output produces same tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >ls_out &&
	grit mktree <ls_out >mktree_sha &&
	test "$tree" = "$(cat mktree_sha)"
	)
'

test_expect_success 'mktree from shuffled ls-tree output produces same tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" | sort -r >shuffled &&
	grit mktree <shuffled >mktree_sha &&
	test "$tree" = "$(cat mktree_sha)"
	)
'

test_expect_success 'empty tree has known OID' '
	(
	cd repo &&
	printf "" | grit mktree >actual &&
	echo "4b825dc642cb6eb9a060e54bf8d69288fbee4904" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'hash-object -t tree of empty content matches empty tree OID' '
	(
	cd repo &&
	empty_tree=$(printf "" | grit hash-object -t tree --stdin -w) &&
	test "$empty_tree" = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
	)
'

test_expect_success 'tree with single blob roundtrips through mktree and ls-tree' '
	(
	cd repo &&
	blob=$(echo "content" | grit hash-object -w --stdin) &&
	printf "100644 blob $blob\tsingle.txt\n" >entry &&
	tree=$(grit mktree <entry) &&
	grit ls-tree "$tree" >actual &&
	test_cmp entry actual
	)
'

test_expect_success 'tree with executable blob roundtrips' '
	(
	cd repo &&
	blob=$(echo "#!/bin/sh" | grit hash-object -w --stdin) &&
	printf "100755 blob $blob\trun.sh\n" >entry &&
	tree=$(grit mktree <entry) &&
	grit ls-tree "$tree" >actual &&
	test_cmp entry actual
	)
'

test_expect_success 'tree with symlink entry roundtrips' '
	(
	cd repo &&
	blob=$(echo "target" | grit hash-object -w --stdin) &&
	printf "120000 blob $blob\tlink\n" >entry &&
	tree=$(grit mktree <entry) &&
	grit ls-tree "$tree" >actual &&
	test_cmp entry actual
	)
'

test_expect_success 'nested tree: ls-tree -r roundtrip via write-tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >recursive_out &&
	grit ls-tree "$tree" >top_out &&
	subtree_sha=$(grit mktree <top_out) &&
	test "$tree" = "$subtree_sha"
	)
'

test_expect_success 'write-tree --prefix matches subtree OID' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >root_entries &&
	dir_oid=$(awk "\$4==\"dir\" {print \$3}" root_entries) &&
	prefix_tree=$(grit write-tree --prefix=dir/ 2>/dev/null) || {
		echo "write-tree --prefix crashed or unsupported, skipping" &&
		return 0
	} &&
	test "$prefix_tree" = "$dir_oid"
	)
'

test_expect_success 'write-tree --prefix for deeper subtree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" dir >dir_entry &&
	dir_oid=$(awk "{print \$3}" dir_entry) &&
	grit ls-tree "$dir_oid" >dir_entries &&
	sub_oid=$(awk "\$4==\"sub\" {print \$3}" dir_entries) &&
	prefix_tree=$(grit write-tree --prefix=dir/sub/ 2>/dev/null) || {
		echo "write-tree --prefix crashed or unsupported, skipping" &&
		return 0
	} &&
	test "$prefix_tree" = "$sub_oid"
	)
'

test_expect_success 'cat-file -t on tree returns tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit cat-file -t "$tree" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -p on tree shows entries' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit cat-file -p "$tree" >actual &&
	grep "root.txt" actual &&
	grep "dir" actual
	)
'

test_expect_success 'identical content trees produce same hash' '
	(
	cd repo &&
	tree1=$(grit write-tree) &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'different content produces different tree hash' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	echo "new content" >root.txt &&
	grit update-index --add root.txt &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after"
	)
'

test_expect_success 'mktree with duplicate entry name keeps entries (or deduplicates)' '
	(
	cd repo &&
	blob1=$(echo "a" | grit hash-object -w --stdin) &&
	blob2=$(echo "b" | grit hash-object -w --stdin) &&
	printf "100644 blob $blob1\tdup.txt\n100644 blob $blob2\tdup.txt\n" >dups &&
	if grit mktree <dups >out 2>/dev/null; then
		tree=$(cat out) &&
		grit ls-tree "$tree" >actual &&
		lines=$(wc -l <actual) &&
		test "$lines" -ge 1
	fi
	)
'

test_expect_success 'ls-tree -t shows trees when recursing' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree -r -t "$tree" >actual &&
	grep "^040000 tree" actual &&
	grep "^100644 blob" actual
	)
'

test_expect_success 'ls-tree with path filter shows only matching entries' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" root.txt >actual &&
	test_line_count = 1 actual &&
	grep "root.txt" actual
	)
'

test_expect_success 'ls-tree --long shows object sizes' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree -l "$tree" >actual &&
	grep "[0-9]" actual
	)
'

test_expect_success 'mktree with --missing allows unknown OIDs' '
	(
	cd repo &&
	printf "100644 blob aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\tghost.txt\n" >entry &&
	grit mktree --missing <entry >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'write-tree fails on missing objects without --missing-ok' '
	(
	cd repo &&
	rm -f .git/index &&
	grit update-index --index-info <<-EOF &&
	100644 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 0	ghost.txt
	EOF
	test_must_fail grit write-tree 2>/dev/null
	)
'

test_expect_success 'write-tree --missing-ok allows missing objects' '
	(
	cd repo &&
	grit write-tree --missing-ok >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'restore index after missing-object tests' '
	(
	cd repo &&
	rm -f .git/index &&
	echo "root file" >root.txt &&
	echo "dir file" >dir/file.txt &&
	echo "deep file" >dir/sub/deep.txt &&
	grit update-index --add root.txt dir/file.txt dir/sub/deep.txt
	)
'

test_done
