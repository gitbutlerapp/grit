#!/bin/sh
# Test grit tag creation, deletion (-d), force (-f), annotated tags,
# listing, sorting, and related operations.

test_description='grit tag delete and force'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with commits' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "first" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "first commit" &&
	grit rev-parse HEAD >../commit1 &&
	echo "second" >>file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "second commit" &&
	grit rev-parse HEAD >../commit2 &&
	echo "third" >>file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "third commit" &&
	grit rev-parse HEAD >../commit3
	)
'

test_expect_success 'create lightweight tag' '
	(
	cd repo &&
	grit tag v1.0
	)
'

test_expect_success 'tag points to HEAD' '
	(
	cd repo &&
	tag_oid=$(grit rev-parse v1.0) &&
	head_oid=$(grit rev-parse HEAD) &&
	test "$tag_oid" = "$head_oid"
	)
'

test_expect_success 'list tags shows created tag' '
	(
	cd repo &&
	grit tag -l >actual &&
	grep "v1.0" actual
	)
'

test_expect_success 'create annotated tag with -m' '
	(
	cd repo &&
	grit tag -m "Release 1.1" v1.1
	)
'

test_expect_success 'annotated tag dereferences to HEAD commit' '
	(
	cd repo &&
	tag_oid=$(grit rev-parse "v1.1^{commit}") &&
	head_oid=$(grit rev-parse HEAD) &&
	test "$tag_oid" = "$head_oid"
	)
'

test_expect_success 'delete lightweight tag with -d' '
	(
	cd repo &&
	grit tag -d v1.0 &&
	grit tag -l >actual &&
	! grep "^v1.0$" actual
	)
'

test_expect_success 'delete annotated tag with -d' '
	(
	cd repo &&
	grit tag -d v1.1 &&
	grit tag -l >actual &&
	! grep "^v1.1$" actual
	)
'

test_expect_success 'delete nonexistent tag fails' '
	(
	cd repo &&
	! grit tag -d nosuch 2>/dev/null
	)
'

test_expect_success 'create tag on specific commit by OID' '
	(
	cd repo &&
	first_oid=$(cat ../commit1) &&
	grit tag v0.1 "$first_oid" &&
	tag_oid=$(grit rev-parse v0.1) &&
	test "$tag_oid" = "$first_oid"
	)
'

test_expect_success 'create tag on HEAD~1' '
	(
	cd repo &&
	second_oid=$(cat ../commit2) &&
	grit tag v0.2 HEAD~1 &&
	tag_oid=$(grit rev-parse v0.2) &&
	test "$tag_oid" = "$second_oid"
	)
'

test_expect_success 'tag -f overwrites existing lightweight tag' '
	(
	cd repo &&
	head_oid=$(grit rev-parse HEAD) &&
	grit tag -f v0.1 HEAD &&
	new_oid=$(grit rev-parse v0.1) &&
	test "$new_oid" = "$head_oid"
	)
'

test_expect_success 'creating duplicate tag without -f fails' '
	(
	cd repo &&
	grit tag dup-tag &&
	! grit tag dup-tag 2>/dev/null
	)
'

test_expect_success 'tag -f on annotated tag to different commit' '
	(
	cd repo &&
	first_oid=$(cat ../commit1) &&
	grit tag -m "old" forced-ann "$first_oid" &&
	grit tag -f -m "new" forced-ann HEAD &&
	new_target=$(grit rev-parse "forced-ann^{commit}") &&
	head_oid=$(grit rev-parse HEAD) &&
	test "$new_target" = "$head_oid"
	)
'

test_expect_success 'list tags after multiple operations' '
	(
	cd repo &&
	grit tag -l >actual &&
	grep "v0.1" actual &&
	grep "v0.2" actual &&
	grep "dup-tag" actual
	)
'

test_expect_success 'tag -l with glob pattern filters tags' '
	(
	cd repo &&
	grit tag -l "v*" >actual &&
	grep "v0.1" actual &&
	grep "v0.2" actual &&
	! grep "dup-tag" actual
	)
'

test_expect_success 'delete and recreate same tag name' '
	(
	cd repo &&
	grit tag temp-tag &&
	grit tag -d temp-tag &&
	first_oid=$(cat ../commit1) &&
	grit tag temp-tag "$first_oid" &&
	tag_oid=$(grit rev-parse temp-tag) &&
	test "$tag_oid" = "$first_oid" &&
	grit tag -d temp-tag
	)
'

test_expect_success 'annotated tag with -a flag' '
	(
	cd repo &&
	grit tag -a -m "annotated v2" v2.0 &&
	grit tag -l >actual &&
	grep "v2.0" actual
	)
'

test_expect_success 'tag -n 1 shows annotation message' '
	(
	cd repo &&
	grit tag -n 1 >actual &&
	grep "v2.0" actual &&
	grep "annotated v2" actual
	)
'

test_expect_success 'tag on earlier commit via saved OID' '
	(
	cd repo &&
	first_oid=$(cat ../commit1) &&
	grit tag side-tag "$first_oid" &&
	tag_oid=$(grit rev-parse side-tag) &&
	test "$tag_oid" = "$first_oid"
	)
'

test_expect_success 'tag --contains lists tags containing commit' '
	(
	cd repo &&
	grit tag --contains HEAD >actual &&
	test_line_count -gt 0 actual
	)
'

test_expect_success 'multiple tags on same commit' '
	(
	cd repo &&
	grit tag multi-a HEAD &&
	grit tag multi-b HEAD &&
	grit tag multi-c HEAD &&
	a_oid=$(grit rev-parse multi-a) &&
	b_oid=$(grit rev-parse multi-b) &&
	c_oid=$(grit rev-parse multi-c) &&
	test "$a_oid" = "$b_oid" &&
	test "$b_oid" = "$c_oid"
	)
'

test_expect_success 'delete one of multiple tags on same commit' '
	(
	cd repo &&
	grit tag -d multi-b &&
	grit tag -l >actual &&
	grep "multi-a" actual &&
	! grep "multi-b" actual &&
	grep "multi-c" actual
	)
'

test_expect_success 'tag with long message via -F file' '
	(
	cd repo &&
	echo "This is a long tag message" >msg.txt &&
	echo "with multiple lines" >>msg.txt &&
	grit tag -F msg.txt v3.0 &&
	grit tag -l >actual &&
	grep "v3.0" actual
	)
'

test_expect_success 'force replace annotated with lightweight' '
	(
	cd repo &&
	grit tag -f v3.0 HEAD &&
	tag_oid=$(grit rev-parse v3.0) &&
	head_oid=$(grit rev-parse HEAD) &&
	test "$tag_oid" = "$head_oid"
	)
'

test_expect_success 'force replace lightweight with annotated' '
	(
	cd repo &&
	grit tag -f -m "now annotated" v3.0 &&
	grit tag -l -n 1 >actual &&
	grep "v3.0" actual &&
	grep "now annotated" actual
	)
'

test_expect_success 'tag listing is sorted alphabetically' '
	(
	cd repo &&
	grit tag -l >actual &&
	sort actual >sorted &&
	test_cmp actual sorted
	)
'

test_expect_success 'tag with --sort=version:refname' '
	(
	cd repo &&
	grit tag -l --sort=version:refname >actual &&
	test_line_count -gt 0 actual
	)
'

test_expect_success 'delete all test tags one by one' '
	(
	cd repo &&
	for t in v0.1 v0.2 v2.0 v3.0 dup-tag forced-ann multi-a multi-c side-tag; do
		grit tag -d "$t" 2>/dev/null || true
	done &&
	grit tag -l >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'create tag after deleting all' '
	(
	cd repo &&
	grit tag fresh &&
	grit tag -l >actual &&
	test_line_count = 1 actual &&
	grep "fresh" actual
	)
'

test_expect_success 'tag with hyphen in name' '
	(
	cd repo &&
	grit tag my-release-v1 &&
	grit tag -l >actual &&
	grep "my-release-v1" actual &&
	grit tag -d my-release-v1
	)
'

test_expect_success 'tag with dots in name' '
	(
	cd repo &&
	grit tag release.2.0.1 &&
	grit tag -l >actual &&
	grep "release.2.0.1" actual &&
	grit tag -d release.2.0.1
	)
'

test_expect_success 'annotated tag message preserved in cat-file' '
	(
	cd repo &&
	grit tag -m "verify message" verify-msg &&
	tag_obj=$(grit rev-parse verify-msg) &&
	grit cat-file -p "$tag_obj" >actual &&
	grep "verify message" actual &&
	grit tag -d verify-msg
	)
'

test_expect_success 'cleanup fresh tag' '
	(
	cd repo &&
	grit tag -d fresh &&
	grit tag -l >actual &&
	test_must_be_empty actual
	)
'

test_done
