#!/bin/sh
# Tests for grit tag: annotated tags, lightweight tags, listing,
# deletion, -n, --contains, --sort, dereferencing, -f, -F.

test_description='grit tag annotated, lightweight, deref, and options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with multiple commits' '
	(
	grit init repo &&
	cd repo &&
	grit config set user.name "Test" &&
	grit config set user.email "test@test.com" &&
	echo "first" >file.txt &&
	grit add file.txt &&
	grit commit -m "first commit" &&
	echo "second" >>file.txt &&
	grit add file.txt &&
	grit commit -m "second commit" &&
	echo "third" >>file.txt &&
	grit add file.txt &&
	grit commit -m "third commit"
	)
'

test_expect_success 'save commit SHAs' '
	(
	cd repo &&
	grit rev-parse HEAD >../sha_head &&
	grit rev-parse HEAD~1 >../sha_parent &&
	grit rev-parse HEAD~2 >../sha_first
	)
'

###########################################################################
# Section 2: Lightweight tags
###########################################################################

test_expect_success 'create lightweight tag' '
	(
	cd repo &&
	grit tag v1.0 &&
	grit tag -l >actual &&
	grep "v1.0" actual
	)
'

test_expect_success 'lightweight tag points to HEAD' '
	(
	cd repo &&
	grit rev-parse v1.0 >actual &&
	test_cmp ../sha_head actual
	)
'

test_expect_success 'create lightweight tag at specific commit' '
	(
	cd repo &&
	grit tag v0.9 $(cat ../sha_parent) &&
	grit rev-parse v0.9 >actual &&
	test_cmp ../sha_parent actual
	)
'

test_expect_success 'create lightweight tag at first commit' '
	(
	cd repo &&
	grit tag v0.1 $(cat ../sha_first) &&
	grit rev-parse v0.1 >actual &&
	test_cmp ../sha_first actual
	)
'

###########################################################################
# Section 3: Annotated tags
###########################################################################

test_expect_success 'create annotated tag with -m' '
	(
	cd repo &&
	grit tag -a -m "Release 2.0" v2.0 &&
	grit tag -l >actual &&
	grep "v2.0" actual
	)
'

test_expect_success 'annotated tag dereferences to HEAD commit' '
	(
	cd repo &&
	grit rev-parse v2.0^{commit} >actual &&
	test_cmp ../sha_head actual
	)
'

test_expect_success 'annotated tag object differs from commit' '
	(
	cd repo &&
	grit rev-parse v2.0 >tag_oid &&
	grit rev-parse v2.0^{commit} >commit_oid &&
	! test_cmp tag_oid commit_oid
	)
'

test_expect_success 'cat-file shows tag type for annotated tag' '
	(
	cd repo &&
	grit cat-file -t v2.0 >actual &&
	echo "tag" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file shows commit type when given deref SHA' '
	(
	cd repo &&
	commit_sha=$(grit rev-parse v2.0^{commit}) &&
	grit cat-file -t "$commit_sha" >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -p shows tag message' '
	(
	cd repo &&
	grit cat-file -p v2.0 >actual &&
	grep "Release 2.0" actual
	)
'

test_expect_success 'cat-file -p shows tagger' '
	(
	cd repo &&
	grit cat-file -p v2.0 >actual &&
	grep "tagger" actual &&
	grep "C O Mitter" actual
	)
'

test_expect_success 'create annotated tag with -m implies -a' '
	(
	cd repo &&
	grit tag -m "Implicit annotated" v2.1 &&
	grit cat-file -t v2.1 >actual &&
	echo "tag" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'create annotated tag at specific commit' '
	(
	cd repo &&
	grit tag -m "Old release" v0.5 $(cat ../sha_first) &&
	grit rev-parse v0.5^{commit} >actual &&
	test_cmp ../sha_first actual
	)
'

###########################################################################
# Section 4: Tag with -F (message from file)
###########################################################################

test_expect_success 'create tag with -F reads message from file' '
	(
	cd repo &&
	echo "Release notes from file" >tagmsg.txt &&
	grit tag -F tagmsg.txt v3.0 &&
	grit cat-file -p v3.0 >actual &&
	grep "Release notes from file" actual
	)
'

###########################################################################
# Section 5: Tag listing
###########################################################################

test_expect_success 'tag -l lists all tags' '
	(
	cd repo &&
	grit tag -l >actual &&
	grep "v1.0" actual &&
	grep "v2.0" actual &&
	grep "v0.9" actual
	)
'

test_expect_success 'tag -l with pattern filters' '
	(
	cd repo &&
	grit tag -l "v2*" >actual &&
	grep "v2.0" actual &&
	grep "v2.1" actual &&
	! grep "v1.0" actual
	)
'

test_expect_success 'tag -l with pattern v0*' '
	(
	cd repo &&
	grit tag -l "v0*" >actual &&
	grep "v0.1" actual &&
	grep "v0.5" actual &&
	grep "v0.9" actual &&
	! grep "v1.0" actual
	)
'

test_expect_success 'tag list is sorted alphabetically' '
	(
	cd repo &&
	grit tag -l >actual &&
	sort actual >sorted &&
	test_cmp sorted actual
	)
'

test_expect_success 'tag -n shows annotation lines' '
	(
	cd repo &&
	grit tag -n >actual &&
	grep "v2.0" actual &&
	grep "Release 2.0" actual
	)
'

test_expect_success 'tag -n1 shows one line of annotation' '
	(
	cd repo &&
	grit tag -n1 >actual &&
	grep "v2.0" actual &&
	grep "Release 2.0" actual
	)
'

###########################################################################
# Section 6: Tag deletion
###########################################################################

test_expect_success 'delete lightweight tag' '
	(
	cd repo &&
	grit tag temp-light &&
	grit tag -d temp-light &&
	grit tag -l >actual &&
	! grep "temp-light" actual
	)
'

test_expect_success 'delete annotated tag' '
	(
	cd repo &&
	grit tag -m "temporary" temp-ann &&
	grit tag -d temp-ann &&
	grit tag -l >actual &&
	! grep "temp-ann" actual
	)
'

test_expect_success 'delete nonexistent tag fails' '
	(
	cd repo &&
	test_must_fail grit tag -d no-such-tag
	)
'

###########################################################################
# Section 7: --force / -f
###########################################################################

test_expect_success 'tag creation fails if tag exists' '
	(
	cd repo &&
	test_must_fail grit tag v1.0
	)
'

test_expect_success 'tag -f overwrites existing tag' '
	(
	cd repo &&
	grit tag -f v1.0 $(cat ../sha_parent) &&
	grit rev-parse v1.0 >actual &&
	test_cmp ../sha_parent actual
	)
'

test_expect_success 'restore v1.0 to HEAD' '
	(
	cd repo &&
	grit tag -f v1.0 HEAD
	)
'

###########################################################################
# Section 8: --contains
###########################################################################

test_expect_success 'tag --contains HEAD lists tags at HEAD' '
	(
	cd repo &&
	grit tag --contains HEAD >actual &&
	grep "v1.0" actual &&
	grep "v2.0" actual
	)
'

test_expect_success 'tag --contains first commit lists all tags' '
	(
	cd repo &&
	grit tag --contains $(cat ../sha_first) >actual &&
	grep "v0.1" actual &&
	grep "v1.0" actual
	)
'

###########################################################################
# Section 9: Comparison with real git
###########################################################################

test_expect_success 'grit and git tag list match' '
	(
	cd repo &&
	grit tag -l >grit_tags &&
	$REAL_GIT tag -l >git_tags &&
	test_cmp git_tags grit_tags
	)
'

test_expect_success 'grit and git agree on lightweight tag deref' '
	(
	cd repo &&
	grit rev-parse v1.0 >grit_v &&
	$REAL_GIT rev-parse v1.0 >git_v &&
	test_cmp git_v grit_v
	)
'

test_expect_success 'grit and git agree on annotated tag deref to commit' '
	(
	cd repo &&
	grit rev-parse v2.0^{commit} >grit_v &&
	$REAL_GIT rev-parse "v2.0^{commit}" >git_v &&
	test_cmp git_v grit_v
	)
'

###########################################################################
# Section 10: Edge cases
###########################################################################

test_expect_success 'tag with hyphen in name' '
	(
	cd repo &&
	grit tag my-release &&
	grit tag -l >actual &&
	grep "my-release" actual &&
	grit tag -d my-release
	)
'

test_expect_success 'tag with dots in name' '
	(
	cd repo &&
	grit tag release.2024.01 &&
	grit tag -l >actual &&
	grep "release.2024.01" actual &&
	grit tag -d release.2024.01
	)
'

test_expect_success 'multiple annotated tags on same commit' '
	(
	cd repo &&
	grit tag -m "first tag" multi-a &&
	grit tag -m "second tag" multi-b &&
	grit rev-parse multi-a^{commit} >a_commit &&
	grit rev-parse multi-b^{commit} >b_commit &&
	test_cmp a_commit b_commit
	)
'

test_expect_success 'show command displays annotated tag info' '
	(
	cd repo &&
	grit show v2.0 >actual &&
	grep "Release 2.0" actual &&
	grep "tag v2.0" actual
	)
'

test_done
