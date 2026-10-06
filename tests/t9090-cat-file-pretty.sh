#!/bin/sh
# Tests for grit cat-file -p (pretty-print) and related flags.

test_description='grit cat-file pretty-print'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

echo_without_newline () {
	printf '%s' "$*"
}

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with various objects' '
	(
	grit init repo &&
	cd repo &&
	echo "hello world" >hello.txt &&
	mkdir -p subdir &&
	echo "nested" >subdir/nested.txt &&
	grit add hello.txt subdir/nested.txt &&
	blob_oid=$(grit hash-object -w hello.txt) &&
	echo "$blob_oid" >../blob_oid &&
	tree_oid=$(grit write-tree) &&
	echo "$tree_oid" >../tree_oid &&
	commit_oid=$(echo "test commit" | grit commit-tree "$tree_oid") &&
	echo "$commit_oid" >../commit_oid
	)
'

###########################################################################
# Section 2: Pretty-print blobs
###########################################################################

test_expect_success 'cat-file -p prints blob content' '
	(
	cd repo &&
	grit cat-file -p "$(cat ../blob_oid)" >actual &&
	echo "hello world" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file blob (positional) prints same content' '
	(
	cd repo &&
	grit cat-file "$(cat ../blob_oid)" >actual &&
	echo "hello world" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -p empty blob prints nothing' '
	(
	cd repo &&
	empty_oid=$(printf "" | grit hash-object -w --stdin) &&
	grit cat-file -p "$empty_oid" >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'cat-file -p binary blob preserves bytes' '
	(
	cd repo &&
	printf "\000\001\377" >binary.dat &&
	bin_oid=$(grit hash-object -w binary.dat) &&
	grit cat-file -p "$bin_oid" >actual &&
	test_cmp binary.dat actual
	)
'

###########################################################################
# Section 3: Pretty-print trees
###########################################################################

test_expect_success 'cat-file -p tree shows entries' '
	(
	cd repo &&
	grit cat-file -p "$(cat ../tree_oid)" >actual &&
	grep "hello.txt" actual &&
	grep "subdir" actual
	)
'

test_expect_success 'cat-file -p tree shows mode and type' '
	(
	cd repo &&
	grit cat-file -p "$(cat ../tree_oid)" >actual &&
	grep "^100644 blob" actual &&
	grep "^040000 tree" actual
	)
'

test_expect_success 'cat-file -p tree entries contain OIDs' '
	(
	cd repo &&
	grit cat-file -p "$(cat ../tree_oid)" >actual &&
	grep -qE "[0-9a-f]{40}" actual
	)
'

test_expect_success 'cat-file -p tree matches ls-tree output' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit cat-file -p "$tree" >cat_out &&
	grit ls-tree "$tree" >ls_out &&
	test_cmp ls_out cat_out
	)
'

###########################################################################
# Section 4: Pretty-print commits
###########################################################################

test_expect_success 'cat-file -p commit shows tree header' '
	(
	cd repo &&
	grit cat-file -p "$(cat ../commit_oid)" >actual &&
	grep "^tree $(cat ../tree_oid)" actual
	)
'

test_expect_success 'cat-file -p commit shows author' '
	(
	cd repo &&
	grit cat-file -p "$(cat ../commit_oid)" >actual &&
	grep "^author " actual
	)
'

test_expect_success 'cat-file -p commit shows committer' '
	(
	cd repo &&
	grit cat-file -p "$(cat ../commit_oid)" >actual &&
	grep "^committer " actual
	)
'

test_expect_success 'cat-file -p commit shows message' '
	(
	cd repo &&
	grit cat-file -p "$(cat ../commit_oid)" >actual &&
	grep "test commit" actual
	)
'

###########################################################################
# Section 5: Type flag (-t)
###########################################################################

test_expect_success 'cat-file -t blob returns blob' '
	(
	cd repo &&
	grit cat-file -t "$(cat ../blob_oid)" >actual &&
	echo blob >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -t tree returns tree' '
	(
	cd repo &&
	grit cat-file -t "$(cat ../tree_oid)" >actual &&
	echo tree >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -t commit returns commit' '
	(
	cd repo &&
	grit cat-file -t "$(cat ../commit_oid)" >actual &&
	echo commit >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 6: Size flag (-s)
###########################################################################

test_expect_success 'cat-file -s blob returns correct size' '
	(
	cd repo &&
	grit cat-file -s "$(cat ../blob_oid)" >actual &&
	expected=$(wc -c <hello.txt | tr -d " ") &&
	echo "$expected" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s empty blob returns 0' '
	(
	cd repo &&
	empty_oid=$(printf "" | grit hash-object -w --stdin) &&
	grit cat-file -s "$empty_oid" >actual &&
	echo 0 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s tree returns non-zero' '
	(
	cd repo &&
	size=$(grit cat-file -s "$(cat ../tree_oid)") &&
	test "$size" -gt 0
	)
'

###########################################################################
# Section 7: Existence check (-e)
###########################################################################

test_expect_success 'cat-file -e succeeds for existing blob' '
	(
	cd repo &&
	grit cat-file -e "$(cat ../blob_oid)"
	)
'

test_expect_success 'cat-file -e succeeds for existing tree' '
	(
	cd repo &&
	grit cat-file -e "$(cat ../tree_oid)"
	)
'

test_expect_success 'cat-file -e succeeds for existing commit' '
	(
	cd repo &&
	grit cat-file -e "$(cat ../commit_oid)"
	)
'

test_expect_success 'cat-file -e fails for nonexistent object' '
	(
	cd repo &&
	test_must_fail grit cat-file -e 0000000000000000000000000000000000000000
	)
'

###########################################################################
# Section 8: Pretty-print tags
###########################################################################

test_expect_success 'cat-file -p tag shows tag content' '
	(
	cd repo &&
	commit=$(cat ../commit_oid) &&
	tag_oid=$(printf "object %s\ntype commit\ntag v1.0\ntagger Test <t@t> 0 +0000\n\nTag message\n" "$commit" | grit mktag) &&
	echo "$tag_oid" >../tag_oid &&
	grit cat-file -p "$tag_oid" >actual &&
	grep "^object $commit" actual &&
	grep "^type commit" actual &&
	grep "^tag v1.0" actual &&
	grep "Tag message" actual
	)
'

test_expect_success 'cat-file -t tag returns tag' '
	(
	cd repo &&
	grit cat-file -t "$(cat ../tag_oid)" >actual &&
	echo tag >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s tag returns non-zero size' '
	(
	cd repo &&
	size=$(grit cat-file -s "$(cat ../tag_oid)") &&
	test "$size" -gt 0
	)
'

###########################################################################
# Section 9: Batch modes
###########################################################################

test_expect_success 'cat-file --batch-check reports type and size' '
	(
	cd repo &&
	oid=$(cat ../blob_oid) &&
	echo "$oid" | grit cat-file --batch-check >actual &&
	grep "$oid" actual &&
	grep "blob" actual
	)
'

test_expect_success 'cat-file --batch prints content after header' '
	(
	cd repo &&
	oid=$(cat ../blob_oid) &&
	echo "$oid" | grit cat-file --batch >actual &&
	grep "hello world" actual
	)
'

test_expect_success 'cat-file --batch-check with multiple OIDs' '
	(
	cd repo &&
	printf "%s\n%s\n" "$(cat ../blob_oid)" "$(cat ../tree_oid)" |
	grit cat-file --batch-check >actual &&
	test_line_count = 2 actual
	)
'

test_done
