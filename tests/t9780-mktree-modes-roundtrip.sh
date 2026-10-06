#!/bin/sh
# Tests for grit mktree with various file modes and round-trip verification.

test_description='grit mktree modes and ls-tree round-trip'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repository with various file modes' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "regular file" >regular.txt &&
	echo "executable" >exec.sh &&
	chmod +x exec.sh &&
	mkdir subdir &&
	echo "in subdir" >subdir/file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial with modes"
	)
'

###########################################################################
# Section 2: Basic mktree from ls-tree output
###########################################################################

test_expect_success 'mktree from ls-tree output reproduces same tree' '
	(
	cd repo &&
	tree_oid=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	grit ls-tree "$tree_oid" | grit mktree >actual_oid &&
	echo "$tree_oid" >expect_oid &&
	test_cmp expect_oid actual_oid
	)
'

test_expect_success 'mktree round-trip: ls-tree -> mktree -> ls-tree is stable' '
	(
	cd repo &&
	tree_oid=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	grit ls-tree "$tree_oid" >first &&
	grit ls-tree "$tree_oid" | grit mktree >new_oid_file &&
	new_oid=$(cat new_oid_file) &&
	grit ls-tree "$new_oid" >second &&
	test_cmp first second
	)
'

test_expect_success 'mktree output matches real git mktree' '
	(
	cd repo &&
	tree_oid=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	"$REAL_GIT" ls-tree "$tree_oid" | grit mktree >actual &&
	"$REAL_GIT" ls-tree "$tree_oid" | "$REAL_GIT" mktree >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 3: Mode 100644 (regular file)
###########################################################################

test_expect_success 'mktree preserves 100644 mode' '
	(
	cd repo &&
	blob_oid=$(grit hash-object -w regular.txt) &&
	printf "100644 blob %s\tregular.txt\n" "$blob_oid" | grit mktree >tree_oid_file &&
	tree_oid=$(cat tree_oid_file) &&
	grit ls-tree "$tree_oid" >actual &&
	grep "^100644" actual
	)
'

test_expect_success 'mktree 100644 blob is readable via cat-file' '
	(
	cd repo &&
	blob_oid=$(grit hash-object -w regular.txt) &&
	printf "100644 blob %s\tregular.txt\n" "$blob_oid" | grit mktree >tree_oid_file &&
	tree_oid=$(cat tree_oid_file) &&
	grit cat-file -t "$tree_oid" >actual &&
	echo tree >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 4: Mode 100755 (executable)
###########################################################################

test_expect_success 'mktree preserves 100755 mode' '
	(
	cd repo &&
	blob_oid=$(grit hash-object -w exec.sh) &&
	printf "100755 blob %s\texec.sh\n" "$blob_oid" | grit mktree >tree_oid_file &&
	tree_oid=$(cat tree_oid_file) &&
	grit ls-tree "$tree_oid" >actual &&
	grep "^100755" actual
	)
'

test_expect_success 'mktree 100755 round-trips through ls-tree' '
	(
	cd repo &&
	blob_oid=$(grit hash-object -w exec.sh) &&
	input="100755 blob ${blob_oid}	exec.sh" &&
	echo "$input" | grit mktree >tree_oid_file &&
	tree_oid=$(cat tree_oid_file) &&
	grit ls-tree "$tree_oid" >actual &&
	echo "$input" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 5: Mode 040000 (tree/subdirectory)
###########################################################################

test_expect_success 'mktree preserves 040000 mode for subtree' '
	(
	cd repo &&
	sub_tree=$("$REAL_GIT" rev-parse HEAD:subdir) &&
	printf "040000 tree %s\tsubdir\n" "$sub_tree" | grit mktree >tree_oid_file &&
	tree_oid=$(cat tree_oid_file) &&
	grit ls-tree "$tree_oid" >actual &&
	grep "^040000" actual
	)
'

test_expect_success 'mktree subtree round-trips correctly' '
	(
	cd repo &&
	sub_tree=$("$REAL_GIT" rev-parse HEAD:subdir) &&
	input="040000 tree ${sub_tree}	subdir" &&
	echo "$input" | grit mktree >tree_oid_file &&
	tree_oid=$(cat tree_oid_file) &&
	grit ls-tree "$tree_oid" >actual &&
	echo "$input" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 6: Mixed modes in single tree
###########################################################################

test_expect_success 'mktree with mixed modes (regular + executable + tree)' '
	(
	cd repo &&
	blob1=$(grit hash-object -w regular.txt) &&
	blob2=$(grit hash-object -w exec.sh) &&
	sub_tree=$("$REAL_GIT" rev-parse HEAD:subdir) &&
	{
		printf "100755 blob %s\texec.sh\n" "$blob2"
		printf "100644 blob %s\tregular.txt\n" "$blob1"
		printf "040000 tree %s\tsubdir\n" "$sub_tree"
	} | grit mktree >tree_oid_file &&
	tree_oid=$(cat tree_oid_file) &&
	grit ls-tree "$tree_oid" >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'mktree mixed-mode tree matches original tree OID' '
	(
	cd repo &&
	orig_tree=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	grit ls-tree "$orig_tree" | grit mktree >reproduced &&
	echo "$orig_tree" >expect &&
	test_cmp expect reproduced
	)
'

test_expect_success 'mktree mixed modes round-trip preserves all modes' '
	(
	cd repo &&
	orig_tree=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	grit ls-tree "$orig_tree" >first &&
	grit ls-tree "$orig_tree" | grit mktree | xargs grit ls-tree >second &&
	test_cmp first second
	)
'

###########################################################################
# Section 7: Multiple round-trips
###########################################################################

test_expect_success 'triple round-trip produces identical tree' '
	(
	cd repo &&
	tree_oid=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	round1=$(grit ls-tree "$tree_oid" | grit mktree) &&
	round2=$(grit ls-tree "$round1" | grit mktree) &&
	round3=$(grit ls-tree "$round2" | grit mktree) &&
	test "$tree_oid" = "$round1" &&
	test "$round1" = "$round2" &&
	test "$round2" = "$round3"
	)
'

###########################################################################
# Section 8: Empty tree
###########################################################################

test_expect_success 'mktree with no input creates empty tree' '
	(
	cd repo &&
	tree_oid=$(echo -n | grit mktree) &&
	grit cat-file -t "$tree_oid" >actual &&
	echo tree >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree empty tree matches real git empty tree' '
	(
	cd repo &&
	grit_empty=$(echo -n | grit mktree) &&
	git_empty=$(echo -n | "$REAL_GIT" mktree) &&
	test "$grit_empty" = "$git_empty"
	)
'

test_expect_success 'mktree empty tree has size 0 reported by cat-file -s' '
	(
	cd repo &&
	tree_oid=$(echo -n | grit mktree) &&
	grit cat-file -s "$tree_oid" >actual &&
	echo 0 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree of empty tree produces no output' '
	(
	cd repo &&
	tree_oid=$(echo -n | grit mktree) &&
	grit ls-tree "$tree_oid" >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 9: Single-entry trees
###########################################################################

test_expect_success 'mktree single blob entry round-trips' '
	(
	cd repo &&
	blob_oid=$(echo "single" | grit hash-object -w --stdin) &&
	input="100644 blob ${blob_oid}	single.txt" &&
	echo "$input" | grit mktree >tree_oid_file &&
	tree_oid=$(cat tree_oid_file) &&
	grit ls-tree "$tree_oid" >actual &&
	echo "$input" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree single tree entry round-trips' '
	(
	cd repo &&
	inner=$(echo -n | grit mktree) &&
	input="040000 tree ${inner}	emptydir" &&
	echo "$input" | grit mktree >tree_oid_file &&
	tree_oid=$(cat tree_oid_file) &&
	grit ls-tree "$tree_oid" >actual &&
	echo "$input" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 10: mktree with sorted entries
###########################################################################

test_expect_success 'mktree entries are sorted in output' '
	(
	cd repo &&
	blob_a=$(echo "aaa" | grit hash-object -w --stdin) &&
	blob_z=$(echo "zzz" | grit hash-object -w --stdin) &&
	{
		printf "100644 blob %s\tzfile.txt\n" "$blob_z"
		printf "100644 blob %s\tafile.txt\n" "$blob_a"
	} | grit mktree >tree_oid_file &&
	tree_oid=$(cat tree_oid_file) &&
	grit ls-tree "$tree_oid" >actual &&
	head -1 actual | grep "afile.txt" &&
	tail -1 actual | grep "zfile.txt"
	)
'

test_expect_success 'mktree sorted output matches real git' '
	(
	cd repo &&
	blob_a=$(echo "aaa" | grit hash-object -w --stdin) &&
	blob_z=$(echo "zzz" | grit hash-object -w --stdin) &&
	input=$(printf "100644 blob %s\tzfile.txt\n100644 blob %s\tafile.txt\n" "$blob_z" "$blob_a") &&
	grit_tree=$(echo "$input" | grit mktree) &&
	git_tree=$(echo "$input" | "$REAL_GIT" mktree) &&
	test "$grit_tree" = "$git_tree"
	)
'

###########################################################################
# Section 11: mktree with many entries
###########################################################################

test_expect_success 'mktree with 20 entries round-trips' '
	(
	cd repo &&
	>entries &&
	for i in $(seq 1 20); do
		blob_oid=$(echo "content $i" | grit hash-object -w --stdin) &&
		printf "100644 blob %s\tfile_%02d.txt\n" "$blob_oid" "$i" >>entries || return 1
	done &&
	tree_oid=$(grit mktree <entries) &&
	grit ls-tree "$tree_oid" >actual &&
	test $(wc -l <actual) -eq 20
	)
'

test_expect_success 'mktree cat-file -t on result is always tree' '
	(
	cd repo &&
	blob_oid=$(echo "check" | grit hash-object -w --stdin) &&
	tree_oid=$(printf "100644 blob %s\tcheck.txt\n" "$blob_oid" | grit mktree) &&
	grit cat-file -t "$tree_oid" >actual &&
	echo tree >expect &&
	test_cmp expect actual
	)
'

test_done
