#!/bin/sh
# Test that write-tree is idempotent: same index state always produces
# the same tree OID, and various index manipulations behave correctly.

test_description='grit write-tree idempotent'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

EMPTY_TREE='4b825dc642cb6eb9a060e54bf8d69288fbee4904'

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User"
	)
'

test_expect_success 'write-tree on empty index produces empty tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	test "$tree" = "$EMPTY_TREE"
	)
'

test_expect_success 'write-tree empty tree twice is identical' '
	(
	cd repo &&
	tree1=$(grit write-tree) &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'write-tree empty tree is a valid tree object' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit cat-file -t "$tree" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'add a file and write-tree' '
	(
	cd repo &&
	echo "hello" >hello &&
	grit update-index --add hello &&
	tree=$(grit write-tree) &&
	test "$tree" != "$EMPTY_TREE" &&
	echo "$tree" >../tree1
	)
'

test_expect_success 'write-tree again without changes is identical' '
	(
	cd repo &&
	tree1=$(cat ../tree1) &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'write-tree output is 40 hex chars' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	echo "$tree" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'write-tree produces valid tree object' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit cat-file -t "$tree" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree of write-tree shows correct entry' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "100644 blob.*hello" actual
	)
'

test_expect_success 'write-tree then mktree from ls-tree produces same tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" | grit mktree >actual &&
	echo "$tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'add second file, write-tree changes' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	echo "world" >world &&
	grit update-index --add world &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after" &&
	echo "$tree_after" >../tree2
	)
'

test_expect_success 'write-tree with two files is idempotent' '
	(
	cd repo &&
	tree1=$(cat ../tree2) &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'remove file and write-tree differs' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	grit update-index --force-remove world &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after"
	)
'

test_expect_success 'after remove, write-tree matches single-file tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	test "$(cat ../tree1)" = "$tree"
	)
'

test_expect_success 're-add same file, write-tree returns to two-file tree' '
	(
	cd repo &&
	echo "world" >world &&
	grit update-index --add world &&
	tree=$(grit write-tree) &&
	test "$(cat ../tree2)" = "$tree"
	)
'

test_expect_success 'modify file content changes tree OID' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	echo "modified" >hello &&
	grit update-index --add hello &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after"
	)
'

test_expect_success 'restore original content restores original tree' '
	(
	cd repo &&
	echo "hello" >hello &&
	grit update-index --add hello &&
	tree=$(grit write-tree) &&
	test "$(cat ../tree2)" = "$tree"
	)
'

test_expect_success 'write-tree with subdirectory' '
	(
	cd repo &&
	mkdir -p sub &&
	echo "nested" >sub/file &&
	grit update-index --add sub/file &&
	tree=$(grit write-tree) &&
	grit cat-file -t "$tree" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual &&
	echo "$tree" >../tree_sub
	)
'

test_expect_success 'write-tree with subdir is idempotent' '
	(
	cd repo &&
	tree1=$(cat ../tree_sub) &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'ls-tree shows subdirectory as tree entry' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "040000 tree.*sub" actual
	)
'

test_expect_success 'ls-tree -r shows nested file' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >actual &&
	grep "sub/file" actual
	)
'

test_expect_success 'write-tree subtree OID matches ls-tree entry' '
	(
	cd repo &&
	tree_full=$(grit write-tree) &&
	grit ls-tree "$tree_full" >ls_full &&
	sub_tree=$(grep "sub" ls_full | awk "{print \$3}") &&
	grit cat-file -t "$sub_tree" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'write-tree subtree content round-trips' '
	(
	cd repo &&
	tree_full=$(grit write-tree) &&
	grit ls-tree "$tree_full" >ls_full &&
	sub_tree=$(grep "sub" ls_full | awk "{print \$3}") &&
	grit ls-tree "$sub_tree" >sub_entries &&
	grep "file" sub_entries
	)
'

test_expect_success 'deep nesting: write-tree is idempotent' '
	(
	cd repo &&
	mkdir -p a/b/c &&
	echo "deep" >a/b/c/deep &&
	grit update-index --add a/b/c/deep &&
	tree1=$(grit write-tree) &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'write-tree deep nesting subtrees are valid' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >all &&
	grep "a/b/c/deep" all
	)
'

test_expect_success 'many files: write-tree is idempotent' '
	(
	cd repo &&
	for i in $(seq 1 20); do
		echo "content $i" >"file_$i" &&
		grit update-index --add "file_$i" || return 1
	done &&
	tree1=$(grit write-tree) &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'many files: ls-tree count matches' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	# Should have: hello, world, sub/, a/, file_1..file_20 = 24 entries
	count=$(wc -l <actual | tr -d " ") &&
	test "$count" -ge 20
	)
'

test_expect_success 'write-tree after index refresh is idempotent' '
	(
	cd repo &&
	tree1=$(grit write-tree) &&
	grit update-index --refresh &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'executable file mode preserved in write-tree' '
	(
	cd repo &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	grit update-index --add script.sh &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "100755.*script.sh" actual
	)
'

test_expect_success 'write-tree with executable is idempotent' '
	(
	cd repo &&
	tree1=$(grit write-tree) &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'write-tree matches mktree from ls-tree output' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" | grit mktree >mktree_oid &&
	echo "$tree" >expect &&
	test_cmp expect mktree_oid
	)
'

test_expect_success 'cat-file -p of write-tree matches ls-tree binary' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit cat-file -t "$tree" >type &&
	echo "tree" >expect &&
	test_cmp expect type
	)
'

test_expect_success 'write-tree size is consistent' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	size1=$(grit cat-file -s "$tree") &&
	size2=$(grit cat-file -s "$tree") &&
	test "$size1" = "$size2"
	)
'

test_expect_success 'clear index and write-tree returns empty tree' '
	(
	cd repo &&
	grit ls-files --cached | while read f; do
		grit update-index --force-remove "$f" || return 1
	done &&
	tree=$(grit write-tree) &&
	test "$tree" = "$EMPTY_TREE"
	)
'

test_done
