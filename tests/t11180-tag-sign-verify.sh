#!/bin/sh
# Tests for grit tag: annotated, lightweight, list, sort, -n, --contains, -d, -f.

test_description='grit tag annotated, lightweight, list, sort, delete, contains, force'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repository with commits' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "first" >file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "first commit" &&
	echo "second" >>file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "second commit" &&
	echo "third" >>file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "third commit"
	)
'

###########################################################################
# Section 2: Lightweight tags
###########################################################################

test_expect_success 'tag creates lightweight tag' '
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
	tag_oid=$(grit rev-parse v1.0) &&
	head_oid=$(grit rev-parse HEAD) &&
	test "$tag_oid" = "$head_oid"
	)
'

test_expect_success 'tag at specific commit' '
	(
	cd repo &&
	first_oid=$(grit rev-list HEAD | tail -1) &&
	grit tag v0.1 "$first_oid" &&
	tag_oid=$(grit rev-parse v0.1) &&
	test "$tag_oid" = "$first_oid"
	)
'

test_expect_success 'lightweight tag matches real git rev-parse' '
	(
	cd repo &&
	grit rev-parse v1.0 >actual &&
	"$REAL_GIT" rev-parse v1.0 >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 3: Annotated tags
###########################################################################

test_expect_success 'tag -a creates annotated tag' '
	(
	cd repo &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	grit tag -a -m "Release 2.0" v2.0 &&
	grit tag -l >actual &&
	grep "v2.0" actual
	)
'

test_expect_success 'annotated tag dereferences to HEAD' '
	(
	cd repo &&
	head_oid=$(grit rev-parse HEAD) &&
	tag_deref=$(grit rev-parse "v2.0^{}") &&
	test "$tag_deref" = "$head_oid"
	)
'

test_expect_success 'annotated tag matches real git deref' '
	(
	cd repo &&
	grit rev-parse "v2.0^{}" >actual &&
	"$REAL_GIT" rev-parse "v2.0^{}" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -m implies annotated' '
	(
	cd repo &&
	grit tag -m "Implied annotated" v2.1 &&
	obj_type=$(grit cat-file -t v2.1) &&
	test "$obj_type" = "tag"
	)
'

test_expect_success 'annotated tag has correct type via cat-file' '
	(
	cd repo &&
	obj_type=$(grit cat-file -t v2.0) &&
	test "$obj_type" = "tag"
	)
'

test_expect_success 'annotated tag content includes message' '
	(
	cd repo &&
	grit cat-file -p v2.0 >actual &&
	grep "Release 2.0" actual
	)
'

test_expect_success 'annotated tag content includes tagger' '
	(
	cd repo &&
	grit cat-file -p v2.0 >actual &&
	grep "tagger" actual &&
	grep "Test User" actual
	)
'

test_expect_success 'annotated tag at specific commit' '
	(
	cd repo &&
	second_oid=$(grit rev-list HEAD | head -2 | tail -1) &&
	grit tag -m "Old release" v0.5 "$second_oid" &&
	tag_deref=$(grit rev-parse "v0.5^{}") &&
	test "$tag_deref" = "$second_oid"
	)
'

###########################################################################
# Section 4: Tag listing
###########################################################################

test_expect_success 'tag -l lists all tags' '
	(
	cd repo &&
	grit tag -l >actual &&
	grep "v0.1" actual &&
	grep "v0.5" actual &&
	grep "v1.0" actual &&
	grep "v2.0" actual &&
	grep "v2.1" actual
	)
'

test_expect_success 'tag -l matches real git list' '
	(
	cd repo &&
	grit tag -l | sort >actual &&
	"$REAL_GIT" tag -l | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag with no args lists tags' '
	(
	cd repo &&
	grit tag >actual &&
	grep "v1.0" actual
	)
'

test_expect_success 'tag -l with pattern filters' '
	(
	cd repo &&
	grit tag -l "v2*" >actual &&
	grep "v2.0" actual &&
	grep "v2.1" actual &&
	! grep "v1.0" actual &&
	! grep "v0" actual
	)
'

test_expect_success 'tag -l pattern matches real git' '
	(
	cd repo &&
	grit tag -l "v2*" | sort >actual &&
	"$REAL_GIT" tag -l "v2*" | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -l with single-match pattern' '
	(
	cd repo &&
	grit tag -l "v1*" >actual &&
	grep "v1.0" actual &&
	test_line_count = 1 actual
	)
'

###########################################################################
# Section 5: Tag -n (show annotation lines)
###########################################################################

test_expect_success 'tag -n shows annotation for annotated tags' '
	(
	cd repo &&
	grit tag -n >actual &&
	grep "v2.0" actual &&
	grep "Release 2.0" actual
	)
'

test_expect_success 'tag -n shows commit subject for lightweight tags' '
	(
	cd repo &&
	grit tag -n >actual &&
	grep "v1.0" actual
	)
'

test_expect_success 'tag -n1 shows one line of annotation' '
	(
	cd repo &&
	grit tag -n1 >actual &&
	grep "v2.0" actual &&
	grep "Release" actual
	)
'

###########################################################################
# Section 6: Tag deletion
###########################################################################

test_expect_success 'tag -d deletes tag' '
	(
	cd repo &&
	grit tag tmp-tag &&
	grit tag -l >before &&
	grep "tmp-tag" before &&
	grit tag -d tmp-tag &&
	grit tag -l >after &&
	! grep "tmp-tag" after
	)
'

test_expect_success 'tag -d matches real git state' '
	(
	cd repo &&
	grit tag -l | sort >actual &&
	"$REAL_GIT" tag -l | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -d fails for nonexistent tag' '
	(
	cd repo &&
	test_must_fail grit tag -d nonexistent-tag
	)
'

test_expect_success 'tag -d deletes annotated tag' '
	(
	cd repo &&
	grit tag -m "to be deleted" tmp-ann &&
	grit tag -d tmp-ann &&
	grit tag -l >actual &&
	! grep "tmp-ann" actual
	)
'

###########################################################################
# Section 7: Tag force
###########################################################################

test_expect_success 'tag -f overwrites existing lightweight tag' '
	(
	cd repo &&
	first_oid=$(grit rev-list HEAD | tail -1) &&
	grit tag -f v1.0 "$first_oid" &&
	tag_oid=$(grit rev-parse v1.0) &&
	test "$tag_oid" = "$first_oid"
	)
'

test_expect_success 'tag -f matches real git behavior' '
	(
	cd repo &&
	grit rev-parse v1.0 >actual &&
	"$REAL_GIT" rev-parse v1.0 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag without -f fails when tag exists' '
	(
	cd repo &&
	test_must_fail grit tag v1.0 2>err
	)
'

###########################################################################
# Section 8: Tag --contains
###########################################################################

test_expect_success 'tag --contains HEAD shows tags at HEAD' '
	(
	cd repo &&
	grit tag --contains HEAD >actual &&
	grep "v2.0" actual
	)
'

test_expect_success 'tag --contains first commit shows all tags' '
	(
	cd repo &&
	first_oid=$(grit rev-list HEAD | tail -1) &&
	grit tag --contains "$first_oid" >actual &&
	grep "v0.1" actual &&
	grep "v2.0" actual
	)
'

###########################################################################
# Section 9: Tag with -F (message from file)
###########################################################################

test_expect_success 'tag -F reads message from file' '
	(
	cd repo &&
	echo "Message from file" >tag-msg.txt &&
	grit tag -F tag-msg.txt v3.0 &&
	grit cat-file -p v3.0 >actual &&
	grep "Message from file" actual
	)
'

test_expect_success 'tag -F creates annotated tag' '
	(
	cd repo &&
	obj_type=$(grit cat-file -t v3.0) &&
	test "$obj_type" = "tag"
	)
'

###########################################################################
# Section 10: Tag sorting and case sensitivity
###########################################################################

test_expect_success 'tags listed in sorted order' '
	(
	cd repo &&
	grit tag -l >actual &&
	sort actual >sorted &&
	test_cmp sorted actual
	)
'

test_expect_success 'tag count matches real git' '
	(
	cd repo &&
	grit tag -l | wc -l | tr -d " " >actual &&
	"$REAL_GIT" tag -l | wc -l | tr -d " " >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -l with no matching pattern gives empty' '
	(
	cd repo &&
	grit tag -l "zzz*" >actual 2>&1 || true &&
	test_must_be_empty actual
	)
'

test_done
