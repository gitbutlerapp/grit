#!/bin/sh
# Tests for grit mktag: tag object creation and validation.

test_description='grit mktag verification'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Helpers
###########################################################################

create_tag_input () {
	cat <<-EOF
	object $1
	type $2
	tag $3
	tagger Test User <test@example.com> 1234567890 +0000

	$4
	EOF
}

# Create a valid commit to tag
setup_commit () {
	grit init repo &&
	cd repo &&
	echo "content" >file.txt &&
	grit add file.txt &&
	tree=$(grit write-tree) &&
	commit=$(echo "initial commit" | grit commit-tree "$tree") &&
	echo "$commit" >../commit_oid &&
	echo "$tree" >../tree_oid
}

###########################################################################
# Section 1: Basic mktag
###########################################################################

test_expect_success 'setup repository with commit' '
	(
	setup_commit
	)
'

test_expect_success 'mktag creates a valid tag object' '
	(
	cd repo &&
	commit=$(cat ../commit_oid) &&
	create_tag_input "$commit" commit v1.0 "Release v1.0" |
	grit mktag >tag_oid &&
	test -s tag_oid
	)
'

test_expect_success 'mktag output is a valid OID (40 hex chars)' '
	(
	cd repo &&
	tag_oid=$(cat tag_oid) &&
	echo "$tag_oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'mktag tag object exists in ODB' '
	(
	cd repo &&
	tag_oid=$(cat tag_oid) &&
	grit cat-file -e "$tag_oid"
	)
'

test_expect_success 'mktag tag object has type tag' '
	(
	cd repo &&
	tag_oid=$(cat tag_oid) &&
	grit cat-file -t "$tag_oid" >actual &&
	echo tag >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktag tag content contains object header' '
	(
	cd repo &&
	tag_oid=$(cat tag_oid) &&
	commit=$(cat ../commit_oid) &&
	grit cat-file -p "$tag_oid" >content &&
	grep "^object $commit" content
	)
'

test_expect_success 'mktag tag content contains type header' '
	(
	cd repo &&
	tag_oid=$(cat tag_oid) &&
	grit cat-file -p "$tag_oid" >content &&
	grep "^type commit" content
	)
'

test_expect_success 'mktag tag content contains tag name' '
	(
	cd repo &&
	tag_oid=$(cat tag_oid) &&
	grit cat-file -p "$tag_oid" >content &&
	grep "^tag v1.0" content
	)
'

test_expect_success 'mktag tag content contains tagger' '
	(
	cd repo &&
	tag_oid=$(cat tag_oid) &&
	grit cat-file -p "$tag_oid" >content &&
	grep "^tagger Test User <test@example.com>" content
	)
'

test_expect_success 'mktag tag content contains message' '
	(
	cd repo &&
	tag_oid=$(cat tag_oid) &&
	grit cat-file -p "$tag_oid" >content &&
	grep "Release v1.0" content
	)
'

###########################################################################
# Section 2: Different target types
###########################################################################

test_expect_success 'mktag can tag a tree object' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	create_tag_input "$tree" tree tree-tag "Tagging a tree" |
	grit mktag >tree_tag_oid &&
	grit cat-file -e "$(cat tree_tag_oid)"
	)
'

test_expect_success 'mktag tree tag has correct type header' '
	(
	cd repo &&
	grit cat-file -p "$(cat tree_tag_oid)" >content &&
	grep "^type tree" content
	)
'

test_expect_success 'mktag can tag a blob object' '
	(
	cd repo &&
	blob_oid=$(grit hash-object -w file.txt) &&
	create_tag_input "$blob_oid" blob blob-tag "Tagging a blob" |
	grit mktag >blob_tag_oid &&
	grit cat-file -e "$(cat blob_tag_oid)"
	)
'

test_expect_success 'mktag blob tag has correct type header' '
	(
	cd repo &&
	grit cat-file -p "$(cat blob_tag_oid)" >content &&
	grep "^type blob" content
	)
'

test_expect_success 'mktag can create tag-of-tag' '
	(
	cd repo &&
	first_tag=$(cat tag_oid) &&
	create_tag_input "$first_tag" tag meta-tag "Tag of a tag" |
	grit mktag >meta_tag_oid &&
	grit cat-file -e "$(cat meta_tag_oid)"
	)
'

test_expect_success 'mktag tag-of-tag has type tag in header' '
	(
	cd repo &&
	grit cat-file -p "$(cat meta_tag_oid)" >content &&
	grep "^type tag" content
	)
'

###########################################################################
# Section 3: Validation / strict mode
###########################################################################

test_expect_success 'mktag rejects missing object header' '
	(
	cd repo &&
	printf "type commit\ntag bad\ntagger T <t@t> 0 +0000\n\nmsg\n" |
	test_must_fail grit mktag 2>err
	)
'

test_expect_success 'mktag rejects missing type header' '
	(
	cd repo &&
	commit=$(cat ../commit_oid) &&
	printf "object %s\ntag bad\ntagger T <t@t> 0 +0000\n\nmsg\n" "$commit" |
	test_must_fail grit mktag 2>err
	)
'

test_expect_success 'mktag rejects missing tag name header' '
	(
	cd repo &&
	commit=$(cat ../commit_oid) &&
	printf "object %s\ntype commit\ntagger T <t@t> 0 +0000\n\nmsg\n" "$commit" |
	test_must_fail grit mktag 2>err
	)
'

test_expect_success 'mktag rejects invalid object OID' '
	(
	cd repo &&
	printf "object invalidhex\ntype commit\ntag bad\ntagger T <t@t> 0 +0000\n\nmsg\n" |
	test_must_fail grit mktag 2>err
	)
'

test_expect_success 'mktag rejects nonexistent object in strict mode' '
	(
	cd repo &&
	fake_oid="0000000000000000000000000000000000000000" &&
	create_tag_input "$fake_oid" commit nonexist "Nonexistent" |
	test_must_fail grit mktag 2>err
	)
'

test_expect_success 'mktag rejects empty input' '
	(
	cd repo &&
	printf "" | test_must_fail grit mktag 2>err
	)
'

###########################################################################
# Section 4: Tag message variations
###########################################################################

test_expect_success 'mktag with empty message body' '
	(
	cd repo &&
	commit=$(cat ../commit_oid) &&
	create_tag_input "$commit" commit empty-msg "" |
	grit mktag >empty_msg_tag &&
	grit cat-file -e "$(cat empty_msg_tag)"
	)
'

test_expect_success 'mktag with multi-line message' '
	(
	cd repo &&
	commit=$(cat ../commit_oid) &&
	msg="Line one
Line two
Line three" &&
	create_tag_input "$commit" commit multi-msg "$msg" |
	grit mktag >multi_tag &&
	grit cat-file -p "$(cat multi_tag)" >content &&
	grep "Line one" content &&
	grep "Line two" content &&
	grep "Line three" content
	)
'

test_expect_success 'mktag with special characters in message' '
	(
	cd repo &&
	commit=$(cat ../commit_oid) &&
	create_tag_input "$commit" commit special-msg "Release! @#$%^&*()" |
	grit mktag >special_tag &&
	grit cat-file -e "$(cat special_tag)"
	)
'

###########################################################################
# Section 5: Determinism and uniqueness
###########################################################################

test_expect_success 'mktag is deterministic for same input' '
	(
	cd repo &&
	commit=$(cat ../commit_oid) &&
	input=$(create_tag_input "$commit" commit det-tag "Deterministic") &&
	oid1=$(echo "$input" | grit mktag) &&
	oid2=$(echo "$input" | grit mktag) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'mktag produces different OIDs for different tag names' '
	(
	cd repo &&
	commit=$(cat ../commit_oid) &&
	oid1=$(create_tag_input "$commit" commit name-a "Same msg" | grit mktag) &&
	oid2=$(create_tag_input "$commit" commit name-b "Same msg" | grit mktag) &&
	test "$oid1" != "$oid2"
	)
'

test_expect_success 'mktag produces different OIDs for different messages' '
	(
	cd repo &&
	commit=$(cat ../commit_oid) &&
	oid1=$(create_tag_input "$commit" commit same-name "Message A" | grit mktag) &&
	oid2=$(create_tag_input "$commit" commit same-name "Message B" | grit mktag) &&
	test "$oid1" != "$oid2"
	)
'

test_done
