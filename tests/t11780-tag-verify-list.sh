#!/bin/sh
#
# Tests for git tag: create, list, delete, annotated, sorting
#

test_description='tag creation, listing, deletion, and annotation'
. ./test-lib.sh

test_expect_success 'setup: create repo with commits' '
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@test.com" &&
	echo "first" >file &&
	git add file &&
	git commit -m "first commit" &&
	echo "second" >>file &&
	git add file &&
	git commit -m "second commit" &&
	echo "third" >>file &&
	git add file &&
	git commit -m "third commit" &&
	cd ..
'

R="$TRASH_DIRECTORY/repo"

test_expect_success 'tag creates lightweight tag' '
	git -C "$R" tag v1.0 &&
	git -C "$R" rev-parse v1.0 >actual &&
	git -C "$R" rev-parse HEAD >expect &&
	test_cmp expect actual
'

test_expect_success 'tag -l lists tags' '
	git -C "$R" tag -l >actual &&
	grep "v1.0" actual
'

test_expect_success 'tag with no args lists tags' '
	git -C "$R" tag >actual &&
	grep "v1.0" actual
'

test_expect_success 'create multiple lightweight tags' '
	git -C "$R" tag v1.1 &&
	git -C "$R" tag v1.2 &&
	git -C "$R" tag >actual &&
	grep "v1.0" actual &&
	grep "v1.1" actual &&
	grep "v1.2" actual
'

test_expect_success 'tag on specific commit' '
	FIRST=$(git -C "$R" rev-parse HEAD~2) &&
	git -C "$R" tag v0.1 "$FIRST" &&
	git -C "$R" rev-parse v0.1 >actual &&
	echo "$FIRST" >expect &&
	test_cmp expect actual
'

test_expect_success 'tag -d deletes tag' '
	git -C "$R" tag delete-me &&
	git -C "$R" tag -d delete-me &&
	git -C "$R" tag >actual &&
	! grep "delete-me" actual
'

test_expect_success 'deleted tag ref is gone' '
	test_must_fail git -C "$R" rev-parse --verify refs/tags/delete-me
'

test_expect_success 'tag -d on nonexistent tag fails' '
	test_must_fail git -C "$R" tag -d nonexistent-tag
'

test_expect_success 'tag -a creates annotated tag' '
	git -C "$R" tag -a -m "Release 2.0" v2.0 &&
	git -C "$R" tag >actual &&
	grep "v2.0" actual
'

test_expect_success 'annotated tag points to correct commit' '
	git -C "$R" rev-parse v2.0^{commit} >actual &&
	git -C "$R" rev-parse HEAD >expect &&
	test_cmp expect actual
'

test_expect_success 'tag -m implies annotated tag' '
	git -C "$R" tag -m "Beta release" v2.0-beta &&
	git -C "$R" cat-file -t v2.0-beta >actual &&
	echo "tag" >expect &&
	test_cmp expect actual
'

test_expect_success 'annotated tag has correct message' '
	git -C "$R" cat-file -p v2.0-beta >actual &&
	grep "Beta release" actual
'

test_expect_success 'annotated tag has tagger info' '
	git -C "$R" cat-file -p v2.0-beta >actual &&
	grep "tagger" actual
'

test_expect_success 'lightweight tag is a commit object' '
	git -C "$R" cat-file -t v1.0 >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'annotated tag is a tag object' '
	git -C "$R" cat-file -t v2.0 >actual &&
	echo "tag" >expect &&
	test_cmp expect actual
'

test_expect_success 'tag -n shows annotation' '
	git -C "$R" tag -n >actual &&
	grep "v2.0" actual
'

test_expect_success 'tag -n1 shows one line of annotation' '
	git -C "$R" tag -n1 >actual &&
	grep "v2.0" actual
'

test_expect_success 'tag -f overwrites existing tag' '
	SECOND=$(git -C "$R" rev-parse HEAD~1) &&
	git -C "$R" tag -f v1.0 "$SECOND" &&
	git -C "$R" rev-parse v1.0 >actual &&
	echo "$SECOND" >expect &&
	test_cmp expect actual
'

test_expect_success 'tag without -f on existing tag fails' '
	test_must_fail git -C "$R" tag v1.0 HEAD
'

test_expect_success 'tag --contains lists tags containing commit' '
	git -C "$R" tag --contains HEAD >actual &&
	grep "v1.1" actual &&
	grep "v1.2" actual &&
	grep "v2.0" actual
'

test_expect_success 'tag --contains with older commit' '
	FIRST=$(git -C "$R" rev-parse HEAD~2) &&
	git -C "$R" tag --contains "$FIRST" >actual &&
	grep "v0.1" actual
'

test_expect_success 'tag -l with pattern' '
	git -C "$R" tag -l "v1.*" >actual &&
	grep "v1.0" actual &&
	grep "v1.1" actual &&
	grep "v1.2" actual &&
	! grep "v2.0" actual
'

test_expect_success 'tag -l with pattern v2*' '
	git -C "$R" tag -l "v2*" >actual &&
	grep "v2.0" actual &&
	! grep "v1.0" actual
'

test_expect_success 'tag with slash in name' '
	git -C "$R" tag release/1.0 &&
	git -C "$R" tag >actual &&
	grep "release/1.0" actual
'

test_expect_success 'tag with dots in name' '
	git -C "$R" tag v3.0.0-rc1 &&
	git -C "$R" tag >actual &&
	grep "v3.0.0-rc1" actual
'

test_expect_success 'tag with hyphen in name' '
	git -C "$R" tag my-special-tag &&
	git -C "$R" tag >actual &&
	grep "my-special-tag" actual
'

test_expect_success 'annotated tag on specific commit' '
	FIRST=$(git -C "$R" rev-parse HEAD~2) &&
	git -C "$R" tag -m "old annotated" annotated-old "$FIRST" &&
	git -C "$R" rev-parse annotated-old^{commit} >actual &&
	echo "$FIRST" >expect &&
	test_cmp expect actual
'

test_expect_success 'delete annotated tag' '
	git -C "$R" tag -a -m "temp" temp-annotated &&
	git -C "$R" tag -d temp-annotated &&
	git -C "$R" tag >actual &&
	! grep "temp-annotated" actual
'

test_expect_success 'tag -F reads message from file' '
	echo "Message from file" >"$TRASH_DIRECTORY/tag-msg.txt" &&
	git -C "$R" tag -F "$TRASH_DIRECTORY/tag-msg.txt" v2.1 &&
	git -C "$R" cat-file -p v2.1 >actual &&
	grep "Message from file" actual
'

test_expect_success 'tag list is sorted alphabetically by default' '
	git -C "$R" tag >actual &&
	sort actual >sorted &&
	test_cmp sorted actual
'

test_expect_success 'tag --sort=version:refname sorts by version' '
	git -C "$R" tag --sort=version:refname >actual &&
	test -s actual
'

test_expect_success 'many tags can be listed' '
	for i in 1 2 3 4 5; do
		git -C "$R" tag "batch-$i" || return 1
	done &&
	git -C "$R" tag >actual &&
	for i in 1 2 3 4 5; do
		grep "batch-$i" actual || return 1
	done
'

test_expect_success 'tag -d removes one of many tags' '
	git -C "$R" tag -d batch-3 &&
	git -C "$R" tag >actual &&
	! grep "batch-3" actual &&
	grep "batch-1" actual &&
	grep "batch-5" actual
'

test_expect_success 'tag -f with annotated tag' '
	SECOND=$(git -C "$R" rev-parse HEAD~1) &&
	git -C "$R" tag -f -m "forced annotated" v2.0 "$SECOND" &&
	git -C "$R" rev-parse v2.0^{commit} >actual &&
	echo "$SECOND" >expect &&
	test_cmp expect actual
'

test_expect_success 'tag -l with no matches returns empty' '
	git -C "$R" tag -l "zzz-no-match*" >actual &&
	test_must_be_empty actual
'

test_done
