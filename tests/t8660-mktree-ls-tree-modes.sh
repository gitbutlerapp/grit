#!/bin/sh
# Tests for mktree with various modes, ls-tree verification, and round-tripping.

test_description='mktree with all modes and ls-tree verification'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

# ── Setup ──────────────────────────────────────────────────────────────────

test_expect_success 'setup: create repository with objects' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "hello" >file.txt &&
	echo "world" >file2.txt &&
	mkdir sub &&
	echo "nested" >sub/deep.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial"
	)
'

# ── Basic mktree ───────────────────────────────────────────────────────────

test_expect_success 'mktree creates a tree from ls-tree output' '
	(
	cd repo &&
	git ls-tree HEAD >ls_out &&
	git mktree <ls_out >actual_tree &&
	expected=$(git rev-parse HEAD^{tree}) &&
	test "$(cat actual_tree)" = "$expected"
	)
'

test_expect_success 'mktree output is a valid 40-hex OID' '
	(
	cd repo &&
	git ls-tree HEAD | git mktree >tree_oid &&
	grep -qE "^[0-9a-f]{40}$" tree_oid
	)
'

test_expect_success 'mktree result is a tree object' '
	(
	cd repo &&
	tree=$(git ls-tree HEAD | git mktree) &&
	type=$(git cat-file -t "$tree") &&
	test "$type" = "tree"
	)
'

# ── Round-trip: ls-tree → mktree → ls-tree ────────────────────────────────

test_expect_success 'ls-tree of mktree output matches original' '
	(
	cd repo &&
	git ls-tree HEAD >original &&
	tree=$(git mktree <original) &&
	git ls-tree "$tree" >roundtrip &&
	test_cmp original roundtrip
	)
'

test_expect_success 'recursive ls-tree round-trips through mktree at top level' '
	(
	cd repo &&
	git ls-tree HEAD >top_level &&
	tree=$(git mktree <top_level) &&
	git ls-tree "$tree" >result &&
	test_cmp top_level result
	)
'

# ── mktree with 100644 mode (regular file) ────────────────────────────────

test_expect_success 'mktree with explicit 100644 blob entry' '
	(
	cd repo &&
	blob=$(echo "test content" | git hash-object -w --stdin) &&
	printf "100644 blob %s\tnewfile.txt\n" "$blob" | git mktree >tree_oid &&
	git ls-tree "$(cat tree_oid)" >actual &&
	grep "100644 blob $blob" actual &&
	grep "newfile.txt" actual
	)
'

test_expect_success 'mktree 100644 entry round-trips correctly' '
	(
	cd repo &&
	blob=$(echo "data" | git hash-object -w --stdin) &&
	printf "100644 blob %s\ttest.txt\n" "$blob" >input &&
	tree=$(git mktree <input) &&
	git ls-tree "$tree" >actual &&
	test_cmp input actual
	)
'

# ── mktree with 100755 mode (executable) ──────────────────────────────────

test_expect_success 'mktree with 100755 executable blob' '
	(
	cd repo &&
	blob=$(echo "#!/bin/sh" | git hash-object -w --stdin) &&
	printf "100755 blob %s\tscript.sh\n" "$blob" | git mktree >tree_oid &&
	git ls-tree "$(cat tree_oid)" >actual &&
	grep "100755 blob $blob" actual
	)
'

test_expect_success 'mktree 100755 preserves mode in ls-tree' '
	(
	cd repo &&
	blob=$(echo "exec" | git hash-object -w --stdin) &&
	printf "100755 blob %s\trun.sh\n" "$blob" >input &&
	tree=$(git mktree <input) &&
	git ls-tree "$tree" >actual &&
	test_cmp input actual
	)
'

# ── mktree with 120000 mode (symlink) ─────────────────────────────────────

test_expect_success 'mktree with 120000 symlink entry' '
	(
	cd repo &&
	blob=$(echo -n "file.txt" | git hash-object -w --stdin) &&
	printf "120000 blob %s\tlink.txt\n" "$blob" | git mktree >tree_oid &&
	git ls-tree "$(cat tree_oid)" >actual &&
	grep "120000 blob $blob" actual
	)
'

test_expect_success 'mktree 120000 round-trips correctly' '
	(
	cd repo &&
	blob=$(echo -n "target" | git hash-object -w --stdin) &&
	printf "120000 blob %s\tsymlink\n" "$blob" >input &&
	tree=$(git mktree <input) &&
	git ls-tree "$tree" >actual &&
	test_cmp input actual
	)
'

# ── mktree with 040000 mode (subtree) ─────────────────────────────────────

test_expect_success 'mktree with 040000 subtree entry' '
	(
	cd repo &&
	sub_tree=$(git rev-parse HEAD:sub) &&
	printf "040000 tree %s\tmydir\n" "$sub_tree" | git mktree >tree_oid &&
	git ls-tree "$(cat tree_oid)" >actual &&
	grep "040000 tree $sub_tree" actual
	)
'

test_expect_success 'mktree subtree round-trips with ls-tree' '
	(
	cd repo &&
	sub_tree=$(git rev-parse HEAD:sub) &&
	printf "040000 tree %s\tsubdir\n" "$sub_tree" >input &&
	tree=$(git mktree <input) &&
	git ls-tree "$tree" >actual &&
	test_cmp input actual
	)
'

# ── mktree with mixed modes ───────────────────────────────────────────────

test_expect_success 'mktree with mixed modes (blob, exec, symlink, tree)' '
	(
	cd repo &&
	blob=$(echo "regular" | git hash-object -w --stdin) &&
	exec_blob=$(echo "#!/bin/sh" | git hash-object -w --stdin) &&
	link_blob=$(echo -n "regular" | git hash-object -w --stdin) &&
	sub_tree=$(git rev-parse HEAD:sub) &&
	{
		printf "100644 blob %s\tfile.txt\n" "$blob"
		printf "100755 blob %s\tscript.sh\n" "$exec_blob"
		printf "120000 blob %s\tlink\n" "$link_blob"
		printf "040000 tree %s\tdir\n" "$sub_tree"
	} | git mktree >tree_oid &&
	git ls-tree "$(cat tree_oid)" >actual &&
	grep "100644" actual &&
	grep "100755" actual &&
	grep "120000" actual &&
	grep "040000" actual
	)
'

test_expect_success 'mktree mixed-mode tree has 4 entries' '
	(
	cd repo &&
	blob=$(echo "a" | git hash-object -w --stdin) &&
	exec_blob=$(echo "b" | git hash-object -w --stdin) &&
	link_blob=$(echo -n "c" | git hash-object -w --stdin) &&
	sub_tree=$(git rev-parse HEAD:sub) &&
	{
		printf "040000 tree %s\td\n" "$sub_tree"
		printf "100644 blob %s\tf.txt\n" "$blob"
		printf "100755 blob %s\tg.sh\n" "$exec_blob"
		printf "120000 blob %s\th\n" "$link_blob"
	} | git mktree >tree_oid &&
	git ls-tree "$(cat tree_oid)" >actual &&
	test_line_count -eq 4 actual
	)
'

# ── mktree from empty input ───────────────────────────────────────────────

test_expect_success 'mktree with empty input creates empty tree' '
	(
	cd repo &&
	empty_tree=$(printf "" | git mktree) &&
	git ls-tree "$empty_tree" >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'mktree empty tree OID matches hash-object empty tree' '
	(
	cd repo &&
	empty_tree=$(printf "" | git mktree) &&
	expected=$(git hash-object -t tree --stdin </dev/null) &&
	test "$empty_tree" = "$expected"
	)
'

# ── mktree --missing ──────────────────────────────────────────────────────

test_expect_success 'mktree --missing accepts non-existent object' '
	(
	cd repo &&
	fake_oid="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" &&
	printf "100644 blob %s\tphantom.txt\n" "$fake_oid" | git mktree --missing >tree_oid &&
	grep -qE "^[0-9a-f]{40}$" tree_oid
	)
'

test_expect_success 'mktree without --missing rejects non-existent object' '
	(
	cd repo &&
	fake_oid="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" &&
	printf "100644 blob %s\tphantom.txt\n" "$fake_oid" >input &&
	test_must_fail git mktree <input
	)
'

# ── mktree --batch ────────────────────────────────────────────────────────

test_expect_success 'mktree --batch creates multiple trees' '
	(
	cd repo &&
	blob1=$(echo "one" | git hash-object -w --stdin) &&
	blob2=$(echo "two" | git hash-object -w --stdin) &&
	{
		printf "100644 blob %s\ta.txt\n" "$blob1"
		printf "\n"
		printf "100644 blob %s\tb.txt\n" "$blob2"
	} | git mktree --batch >trees &&
	test_line_count -eq 2 trees
	)
'

test_expect_success 'mktree --batch trees are independent' '
	(
	cd repo &&
	blob1=$(echo "alpha" | git hash-object -w --stdin) &&
	blob2=$(echo "beta" | git hash-object -w --stdin) &&
	{
		printf "100644 blob %s\ta.txt\n" "$blob1"
		printf "\n"
		printf "100644 blob %s\tb.txt\n" "$blob2"
	} | git mktree --batch >trees &&
	tree1=$(sed -n "1p" trees) &&
	tree2=$(sed -n "2p" trees) &&
	test "$tree1" != "$tree2" &&
	git ls-tree "$tree1" >out1 &&
	git ls-tree "$tree2" >out2 &&
	grep "a.txt" out1 &&
	! grep "a.txt" out2 &&
	grep "b.txt" out2 &&
	! grep "b.txt" out1
	)
'

# ── mktree -z (NUL-terminated input) ──────────────────────────────────────

test_expect_success 'mktree -z reads NUL-terminated input' '
	(
	cd repo &&
	blob=$(echo "nul test" | git hash-object -w --stdin) &&
	printf "100644 blob %s\tnulfile.txt\0" "$blob" | git mktree -z >tree_oid &&
	git ls-tree "$(cat tree_oid)" >actual &&
	grep "nulfile.txt" actual
	)
'

# ── ls-tree verification of modes ─────────────────────────────────────────

test_expect_success 'ls-tree correctly reports 100644 mode' '
	(
	cd repo &&
	git ls-tree HEAD >actual &&
	grep "^100644 " actual
	)
'

test_expect_success 'ls-tree mode field is always 6 digits' '
	(
	cd repo &&
	git ls-tree HEAD >actual &&
	awk "{print \$1}" actual >modes &&
	while read mode; do
		test ${#mode} -eq 6 ||
			{ echo "mode not 6 chars: $mode"; return 1; }
	done <modes
	)
'

test_expect_success 'ls-tree OIDs from mktree match original OIDs' '
	(
	cd repo &&
	blob=$(echo "verify" | git hash-object -w --stdin) &&
	printf "100644 blob %s\tv.txt\n" "$blob" >input &&
	tree=$(git mktree <input) &&
	ls_oid=$(git ls-tree "$tree" | awk "{print \$3}") &&
	test "$ls_oid" = "$blob"
	)
'

test_done
