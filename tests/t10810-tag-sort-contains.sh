#!/bin/sh
# Test grit tag: creation, listing, deletion, sorting, annotation,
# --contains, pattern matching, and force creation.

test_description='grit tag sort, contains, and listing'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with commits' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "A" >file.txt &&
	grit add file.txt &&
	grit commit -m "first" &&
	echo "B" >>file.txt &&
	grit add file.txt &&
	grit commit -m "second" &&
	echo "C" >>file.txt &&
	grit add file.txt &&
	grit commit -m "third"
	)
'

###########################################################################
# Section 2: Basic tag creation
###########################################################################

test_expect_success 'create lightweight tag' '
	(
	cd repo &&
	grit tag v1.0 &&
	grit tag -l >out &&
	grep "v1.0" out
	)
'

test_expect_success 'tag at specific commit' '
	(
	cd repo &&
	first=$(grit rev-list --reverse HEAD | head -1) &&
	grit tag v0.1 "$first" &&
	grit rev-parse v0.1 >out &&
	echo "$first" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'create annotated tag with -m' '
	(
	cd repo &&
	grit tag -m "Release 2.0" v2.0 &&
	grit tag -l >out &&
	grep "v2.0" out
	)
'

test_expect_success 'annotated tag with -a -m' '
	(
	cd repo &&
	grit tag -a -m "Annotated tag" v2.1 &&
	grit tag -l >out &&
	grep "v2.1" out
	)
'

test_expect_success 'tag creation matches git (lightweight)' '
	(
	cd repo &&
	grit tag grit-lt &&
	git tag git-lt &&
	grit rev-parse grit-lt >grit_out &&
	git rev-parse git-lt >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'duplicate tag creation fails' '
	(
	cd repo &&
	grit tag dup-tag &&
	test_must_fail grit tag dup-tag
	)
'

###########################################################################
# Section 3: Tag listing
###########################################################################

test_expect_success 'tag -l lists all tags' '
	(
	cd repo &&
	grit tag -l >out &&
	grep "v0.1" out &&
	grep "v1.0" out &&
	grep "v2.0" out
	)
'

test_expect_success 'tag --list lists all tags' '
	(
	cd repo &&
	grit tag --list >out &&
	grep "v0.1" out &&
	grep "v1.0" out
	)
'

test_expect_success 'tag with no args lists tags' '
	(
	cd repo &&
	grit tag >out &&
	grep "v1.0" out
	)
'

test_expect_success 'tag list is sorted' '
	(
	cd repo &&
	grit tag -l >out &&
	sort out >sorted &&
	test_cmp sorted out
	)
'

test_expect_success 'tag list matches git' '
	(
	cd repo &&
	grit tag -l >grit_out &&
	git tag -l >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 4: Pattern matching
###########################################################################

test_expect_success 'tag -l with pattern filters tags' '
	(
	cd repo &&
	grit tag -l "v1*" >out &&
	grep "v1.0" out &&
	! grep "v2.0" out
	)
'

test_expect_success 'tag -l with v2* pattern' '
	(
	cd repo &&
	grit tag -l "v2*" >out &&
	grep "v2.0" out &&
	grep "v2.1" out &&
	! grep "v1.0" out
	)
'

test_expect_success 'tag -l pattern matches git' '
	(
	cd repo &&
	grit tag -l "v1*" >grit_out &&
	git tag -l "v1*" >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'tag -l with no-match pattern returns empty' '
	(
	cd repo &&
	grit tag -l "zzz*" >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 5: Tag deletion
###########################################################################

test_expect_success 'delete a tag with -d' '
	(
	cd repo &&
	grit tag del-me &&
	grit tag -d del-me &&
	grit tag -l >out &&
	! grep "del-me" out
	)
'

test_expect_success 'delete with --delete' '
	(
	cd repo &&
	grit tag del-me2 &&
	grit tag --delete del-me2 &&
	grit tag -l >out &&
	! grep "del-me2" out
	)
'

test_expect_success 'deleting nonexistent tag fails' '
	(
	cd repo &&
	test_must_fail grit tag -d nonexistent-tag
	)
'

test_expect_success 'delete does not affect other tags' '
	(
	cd repo &&
	grit tag temp-a &&
	grit tag temp-b &&
	grit tag -d temp-a &&
	grit tag -l >out &&
	grep "temp-b" out
	)
'

###########################################################################
# Section 6: Force creation
###########################################################################

test_expect_success 'tag --force overwrites existing tag' '
	(
	cd repo &&
	first=$(grit rev-list --reverse HEAD | head -1) &&
	grit tag force-tag "$first" &&
	grit tag --force force-tag HEAD &&
	grit rev-parse force-tag >out &&
	grit rev-parse HEAD >expect &&
	test_cmp expect out
	)
'

test_expect_success 'tag -f overwrites existing tag' '
	(
	cd repo &&
	first=$(grit rev-list --reverse HEAD | head -1) &&
	grit tag -f force-tag "$first" &&
	grit rev-parse force-tag >out &&
	echo "$first" >expect &&
	test_cmp expect out
	)
'

###########################################################################
# Section 7: --sort
###########################################################################

test_expect_success 'tag --sort=refname lists alphabetically' '
	(
	cd repo &&
	grit tag --sort=refname -l >out &&
	sort out >sorted &&
	test_cmp sorted out
	)
'

test_expect_success 'tag --sort=version:refname sorts by version' '
	(
	cd repo &&
	grit tag --sort=version:refname -l "v*" >out &&
	head -1 out >first_line &&
	grep "v0.1" first_line
	)
'

###########################################################################
# Section 8: --contains
###########################################################################

test_expect_success '--contains HEAD shows tags at HEAD' '
	(
	cd repo &&
	grit tag --contains HEAD >out &&
	grep "v1.0" out
	)
'

test_expect_success '--contains first commit shows all tags' '
	(
	cd repo &&
	first=$(grit rev-list --reverse HEAD | head -1) &&
	grit tag --contains "$first" >out &&
	grep "v0.1" out
	)
'

###########################################################################
# Section 9: Annotated tag details
###########################################################################

test_expect_success 'show annotated tag with cat-file' '
	(
	cd repo &&
	grit cat-file -t v2.0 >out &&
	echo "tag" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'show annotated tag content' '
	(
	cd repo &&
	grit cat-file -p v2.0 >out &&
	grep "Release 2.0" out
	)
'

test_expect_success 'lightweight tag points directly to commit' '
	(
	cd repo &&
	grit cat-file -t v1.0 >out &&
	echo "commit" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'annotated tag tagger line matches git' '
	(
	cd repo &&
	grit cat-file -p v2.0 >grit_out &&
	git cat-file -p v2.0 >git_out &&
	grep "tagger" grit_out &&
	grep "tagger" git_out
	)
'

###########################################################################
# Section 10: Tag with -n (annotation lines)
###########################################################################

test_expect_success 'tag -n shows annotation for annotated tags' '
	(
	cd repo &&
	grit tag -n -l >out &&
	grep "v2.0" out &&
	grep "Release 2.0" out
	)
'

test_expect_success 'tag -n1 shows one line of annotation' '
	(
	cd repo &&
	grit tag -n1 -l "v2*" >out &&
	grep "Release 2.0" out
	)
'

test_done
