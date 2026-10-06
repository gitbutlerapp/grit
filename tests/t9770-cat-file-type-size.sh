#!/bin/sh
# Tests for grit cat-file focusing on -t (type) and -s (size) flags.

test_description='grit cat-file type and size queries'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repository with commits, trees, blobs, tags' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "blob content" >file.txt &&
	mkdir sub &&
	echo "nested" >sub/nested.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial commit" &&
	"$REAL_GIT" tag -a -m "v1 tag" v1 &&
	echo "second" >second.txt &&
	"$REAL_GIT" add second.txt &&
	"$REAL_GIT" commit -m "second commit"
	)
'

###########################################################################
# Section 2: cat-file -t (type detection)
###########################################################################

test_expect_success 'cat-file -t blob reports blob' '
	(
	cd repo &&
	blob_oid=$(grit hash-object file.txt) &&
	grit cat-file -t "$blob_oid" >actual &&
	echo blob >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -t commit reports commit' '
	(
	cd repo &&
	commit_oid=$("$REAL_GIT" rev-parse HEAD) &&
	grit cat-file -t "$commit_oid" >actual &&
	echo commit >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -t tree reports tree' '
	(
	cd repo &&
	tree_oid=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	grit cat-file -t "$tree_oid" >actual &&
	echo tree >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -t tag reports tag' '
	(
	cd repo &&
	tag_oid=$("$REAL_GIT" rev-parse v1) &&
	grit cat-file -t "$tag_oid" >actual &&
	echo tag >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -t matches real git for blob' '
	(
	cd repo &&
	blob_oid=$("$REAL_GIT" hash-object file.txt) &&
	grit cat-file -t "$blob_oid" >actual &&
	"$REAL_GIT" cat-file -t "$blob_oid" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -t matches real git for commit' '
	(
	cd repo &&
	commit_oid=$("$REAL_GIT" rev-parse HEAD) &&
	grit cat-file -t "$commit_oid" >actual &&
	"$REAL_GIT" cat-file -t "$commit_oid" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -t matches real git for tree' '
	(
	cd repo &&
	tree_oid=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	grit cat-file -t "$tree_oid" >actual &&
	"$REAL_GIT" cat-file -t "$tree_oid" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 3: cat-file -s (size queries)
###########################################################################

test_expect_success 'cat-file -s blob matches real git' '
	(
	cd repo &&
	blob_oid=$("$REAL_GIT" hash-object file.txt) &&
	grit cat-file -s "$blob_oid" >actual &&
	"$REAL_GIT" cat-file -s "$blob_oid" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s commit matches real git' '
	(
	cd repo &&
	commit_oid=$("$REAL_GIT" rev-parse HEAD) &&
	grit cat-file -s "$commit_oid" >actual &&
	"$REAL_GIT" cat-file -s "$commit_oid" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s tree matches real git' '
	(
	cd repo &&
	tree_oid=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	grit cat-file -s "$tree_oid" >actual &&
	"$REAL_GIT" cat-file -s "$tree_oid" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s tag matches real git' '
	(
	cd repo &&
	tag_oid=$("$REAL_GIT" rev-parse v1) &&
	grit cat-file -s "$tag_oid" >actual &&
	"$REAL_GIT" cat-file -s "$tag_oid" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s empty blob returns 0' '
	(
	cd repo &&
	oid=$(grit hash-object -w /dev/null) &&
	grit cat-file -s "$oid" >actual &&
	echo 0 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s blob size matches wc -c' '
	(
	cd repo &&
	echo "size verification" >sv.txt &&
	oid=$(grit hash-object -w sv.txt) &&
	grit cat-file -s "$oid" >actual &&
	wc -c <sv.txt | tr -d " " >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 4: cat-file -p (pretty print) basics
###########################################################################

test_expect_success 'cat-file -p blob matches file content' '
	(
	cd repo &&
	oid=$(grit hash-object -w file.txt) &&
	grit cat-file -p "$oid" >actual &&
	test_cmp file.txt actual
	)
'

test_expect_success 'cat-file -p commit matches real git' '
	(
	cd repo &&
	commit_oid=$("$REAL_GIT" rev-parse HEAD) &&
	grit cat-file -p "$commit_oid" >actual &&
	"$REAL_GIT" cat-file -p "$commit_oid" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -p tree matches real git' '
	(
	cd repo &&
	tree_oid=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	grit cat-file -p "$tree_oid" >actual &&
	"$REAL_GIT" cat-file -p "$tree_oid" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -p tag matches real git' '
	(
	cd repo &&
	tag_oid=$("$REAL_GIT" rev-parse v1) &&
	grit cat-file -p "$tag_oid" >actual &&
	"$REAL_GIT" cat-file -p "$tag_oid" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 5: cat-file -e (existence check)
###########################################################################

test_expect_success 'cat-file -e succeeds for existing blob' '
	(
	cd repo &&
	oid=$(grit hash-object -w file.txt) &&
	grit cat-file -e "$oid"
	)
'

test_expect_success 'cat-file -e succeeds for existing commit' '
	(
	cd repo &&
	commit_oid=$("$REAL_GIT" rev-parse HEAD) &&
	grit cat-file -e "$commit_oid"
	)
'

test_expect_success 'cat-file -e succeeds for existing tree' '
	(
	cd repo &&
	tree_oid=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	grit cat-file -e "$tree_oid"
	)
'

test_expect_success 'cat-file -e fails for nonexistent object' '
	(
	cd repo &&
	test_must_fail grit cat-file -e 0000000000000000000000000000000000000000
	)
'

test_expect_success 'cat-file -e produces no output on success' '
	(
	cd repo &&
	oid=$(grit hash-object -w file.txt) &&
	grit cat-file -e "$oid" >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 6: cat-file with nested tree objects
###########################################################################

test_expect_success 'cat-file -t on subtree reports tree' '
	(
	cd repo &&
	sub_tree=$("$REAL_GIT" rev-parse HEAD:sub) &&
	grit cat-file -t "$sub_tree" >actual &&
	echo tree >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s on subtree matches real git' '
	(
	cd repo &&
	sub_tree=$("$REAL_GIT" rev-parse HEAD:sub) &&
	grit cat-file -s "$sub_tree" >actual &&
	"$REAL_GIT" cat-file -s "$sub_tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -p on subtree lists sub entries' '
	(
	cd repo &&
	sub_tree=$("$REAL_GIT" rev-parse HEAD:sub) &&
	grit cat-file -p "$sub_tree" >actual &&
	grep "nested.txt" actual
	)
'

###########################################################################
# Section 7: cat-file error handling
###########################################################################

test_expect_success 'cat-file -t fails for nonexistent object' '
	(
	cd repo &&
	test_must_fail grit cat-file -t 0000000000000000000000000000000000000000
	)
'

test_expect_success 'cat-file -s fails for nonexistent object' '
	(
	cd repo &&
	test_must_fail grit cat-file -s 0000000000000000000000000000000000000000
	)
'

test_expect_success 'cat-file -p fails for nonexistent object' '
	(
	cd repo &&
	test_must_fail grit cat-file -p 0000000000000000000000000000000000000000
	)
'

###########################################################################
# Section 8: Type and size consistency
###########################################################################

test_expect_success 'cat-file -t and -s agree with -p content length for blob' '
	(
	cd repo &&
	echo "consistency check" >cc.txt &&
	oid=$(grit hash-object -w cc.txt) &&
	grit cat-file -t "$oid" >type_out &&
	echo blob >expect_type &&
	test_cmp expect_type type_out &&
	grit cat-file -s "$oid" >size_out &&
	grit cat-file -p "$oid" | wc -c | tr -d " " >computed_size &&
	test_cmp size_out computed_size
	)
'

test_expect_success 'cat-file -t and -s agree for second commit' '
	(
	cd repo &&
	commit_oid=$("$REAL_GIT" rev-parse HEAD) &&
	grit cat-file -t "$commit_oid" >type_out &&
	echo commit >expect_type &&
	test_cmp expect_type type_out &&
	grit cat-file -s "$commit_oid" >size_out &&
	grit cat-file -p "$commit_oid" | wc -c | tr -d " " >computed_size &&
	test_cmp size_out computed_size
	)
'

test_expect_success 'cat-file -s returns numeric value for all object types' '
	(
	cd repo &&
	blob_oid=$("$REAL_GIT" hash-object file.txt) &&
	commit_oid=$("$REAL_GIT" rev-parse HEAD) &&
	tree_oid=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	for oid in "$blob_oid" "$commit_oid" "$tree_oid"; do
		size=$(grit cat-file -s "$oid") &&
		echo "$size" | grep -qE "^[0-9]+$" ||
			{ echo "non-numeric size for $oid: $size"; return 1; }
	done
	)
'

test_done
