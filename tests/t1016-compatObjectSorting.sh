#!/bin/sh
# Tests for object sorting compatibility — trees, mktree, ls-tree ordering.

test_description='grit object sorting compatibility'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	git init repo &&
	cd repo
	)
'

test_expect_success 'mktree sorts entries by name' '
	(
	cd repo &&
	oid_a=$(echo aa | git hash-object -w --stdin) &&
	oid_b=$(echo bb | git hash-object -w --stdin) &&
	tree=$(printf "100644 blob %s\tb\n100644 blob %s\ta\n" "$oid_b" "$oid_a" | git mktree) &&
	git ls-tree $tree >actual &&
	printf "100644 blob %s\ta\n100644 blob %s\tb\n" "$oid_a" "$oid_b" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree with single entry' '
	(
	cd repo &&
	oid=$(echo only | git hash-object -w --stdin) &&
	tree=$(printf "100644 blob %s\tonly\n" "$oid" | git mktree) &&
	git ls-tree $tree >actual &&
	printf "100644 blob %s\tonly\n" "$oid" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree with tree entry sorts correctly' '
	(
	cd repo &&
	oid=$(echo content | git hash-object -w --stdin) &&
	subtree=$(printf "100644 blob %s\tinner\n" "$oid" | git mktree) &&
	tree=$(printf "100644 blob %s\tfile\n040000 tree %s\tdir\n" "$oid" "$subtree" | git mktree) &&
	git ls-tree $tree >actual &&
	printf "040000 tree %s\tdir\n100644 blob %s\tfile\n" "$subtree" "$oid" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tree entries: directories sort with trailing slash' '
	(
	cd repo &&
	oid=$(echo x | git hash-object -w --stdin) &&
	subtree=$(printf "100644 blob %s\tx\n" "$oid" | git mktree) &&
	tree=$(printf "100644 blob %s\tab\n040000 tree %s\ta\n" "$oid" "$subtree" | git mktree) &&
	git ls-tree $tree >actual &&
	head -1 actual >first &&
	grep "a$" first
	)
'

test_expect_success 'ls-tree output has correct format' '
	(
	cd repo &&
	oid=$(echo fmt | git hash-object -w --stdin) &&
	tree=$(printf "100644 blob %s\ttest.txt\n" "$oid" | git mktree) &&
	git ls-tree $tree >actual &&
	grep "^100644 blob $oid	test.txt$" actual
	)
'

test_expect_success 'mktree produces deterministic OIDs' '
	(
	cd repo &&
	oid=$(echo det | git hash-object -w --stdin) &&
	tree1=$(printf "100644 blob %s\tfile\n" "$oid" | git mktree) &&
	tree2=$(printf "100644 blob %s\tfile\n" "$oid" | git mktree) &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'different content produces different tree OIDs' '
	(
	cd repo &&
	oid1=$(echo one | git hash-object -w --stdin) &&
	oid2=$(echo two | git hash-object -w --stdin) &&
	tree1=$(printf "100644 blob %s\tfile\n" "$oid1" | git mktree) &&
	tree2=$(printf "100644 blob %s\tfile\n" "$oid2" | git mktree) &&
	test "$tree1" != "$tree2"
	)
'

test_expect_success 'mktree with executable file mode' '
	(
	cd repo &&
	oid=$(echo exec | git hash-object -w --stdin) &&
	tree=$(printf "100755 blob %s\tscript.sh\n" "$oid" | git mktree) &&
	git ls-tree $tree >actual &&
	grep "^100755 blob" actual
	)
'

test_expect_success 'mktree with symlink mode' '
	(
	cd repo &&
	oid=$(echo target | git hash-object -w --stdin) &&
	tree=$(printf "120000 blob %s\tlink\n" "$oid" | git mktree) &&
	git ls-tree $tree >actual &&
	grep "^120000 blob" actual
	)
'

test_expect_success 'cat-file -p on tree shows sorted entries' '
	(
	cd repo &&
	oid_a=$(echo aa | git hash-object -w --stdin) &&
	oid_b=$(echo bb | git hash-object -w --stdin) &&
	tree=$(printf "100644 blob %s\tz\n100644 blob %s\ta\n" "$oid_b" "$oid_a" | git mktree) &&
	git cat-file -p $tree >actual &&
	head -1 actual >first &&
	grep "	a$" first
	)
'

test_expect_success 'cat-file -t on tree says tree' '
	(
	cd repo &&
	oid=$(echo t | git hash-object -w --stdin) &&
	tree=$(printf "100644 blob %s\tf\n" "$oid" | git mktree) &&
	echo tree >expect &&
	git cat-file -t $tree >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s on tree gives size' '
	(
	cd repo &&
	oid=$(echo sz | git hash-object -w --stdin) &&
	tree=$(printf "100644 blob %s\tf\n" "$oid" | git mktree) &&
	git cat-file -s $tree >actual &&
	test -s actual
	)
'

test_expect_success 'mktree with many entries sorts them' '
	(
	cd repo &&
	oid=$(echo multi | git hash-object -w --stdin) &&
	tree=$(printf "100644 blob %s\tz\n100644 blob %s\tm\n100644 blob %s\ta\n100644 blob %s\tb\n" \
		"$oid" "$oid" "$oid" "$oid" | git mktree) &&
	git ls-tree $tree >actual &&
	cut -f2 actual >names &&
	sort names >sorted &&
	test_cmp sorted names
	)
'

test_expect_success 'mktree empty input produces empty tree' '
	(
	cd repo &&
	tree=$(printf "" | git mktree) &&
	git ls-tree $tree >actual &&
	! test -s actual
	)
'

test_expect_success 'nested trees maintain sorting at each level' '
	(
	cd repo &&
	oid=$(echo deep | git hash-object -w --stdin) &&
	inner=$(printf "100644 blob %s\tb\n100644 blob %s\ta\n" "$oid" "$oid" | git mktree) &&
	outer=$(printf "040000 tree %s\tsub\n100644 blob %s\ttop\n" "$inner" "$oid" | git mktree) &&
	git ls-tree $outer >actual_outer &&
	head -1 actual_outer >first_outer &&
	grep "sub$" first_outer &&
	git ls-tree $inner >actual_inner &&
	head -1 actual_inner >first_inner &&
	grep "	a$" first_inner
	)
'

test_expect_success 'diff-tree between sorted trees works' '
	(
	cd repo &&
	oid_a=$(echo "ver1" | git hash-object -w --stdin) &&
	oid_b=$(echo "ver2" | git hash-object -w --stdin) &&
	tree1=$(printf "100644 blob %s\tfile\n" "$oid_a" | git mktree) &&
	tree2=$(printf "100644 blob %s\tfile\n" "$oid_b" | git mktree) &&
	git diff-tree $tree1 $tree2 >actual &&
	grep "M" actual &&
	grep "file" actual
	)
'

test_expect_success 'diff-tree shows no diff for identical trees' '
	(
	cd repo &&
	oid=$(echo same | git hash-object -w --stdin) &&
	tree=$(printf "100644 blob %s\tfile\n" "$oid" | git mktree) &&
	git diff-tree $tree $tree >actual &&
	! test -s actual
	)
'

test_expect_success 'read-tree loads tree entries into index' '
	(
	cd repo &&
	oid=$(echo rd | git hash-object -w --stdin) &&
	tree=$(printf "100644 blob %s\tread-test\n" "$oid" | git mktree) &&
	git read-tree $tree &&
	git ls-files --stage read-test >actual &&
	grep "$oid" actual
	)
'

test_done
