#!/bin/sh
# Tests for tag creation, listing, sorting (--sort), deletion,
# annotated tags, version:refname sort, pattern matching, --contains.

test_description='tag list, sort, annotated, delete, contains'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- setup ------------------------------------------------------------------

test_expect_success 'setup: create repo with several commits' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "v1" >file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "first" &&
	echo "v2" >file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "second" &&
	echo "v3" >file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "third"
	)
'

# -- lightweight tags --------------------------------------------------------

test_expect_success 'tag creates lightweight tag' '
	(
	cd repo &&
	grit tag v1.0 HEAD~2 &&
	grit tag -l >actual &&
	grep "v1.0" actual
	)
'

test_expect_success 'tag at HEAD' '
	(
	cd repo &&
	grit tag v3.0 &&
	grit tag -l >actual &&
	grep "v3.0" actual
	)
'

test_expect_success 'tag points to correct commit' '
	(
	cd repo &&
	grit rev-parse v1.0 >actual &&
	grit rev-parse HEAD~2 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag v3.0 points to HEAD' '
	(
	cd repo &&
	grit rev-parse v3.0 >actual &&
	grit rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

# -- annotated tags ----------------------------------------------------------

test_expect_success 'tag -a creates annotated tag' '
	(
	cd repo &&
	grit tag -a -m "Release 2.0" v2.0 HEAD~1 &&
	grit tag -l >actual &&
	grep "v2.0" actual
	)
'

test_expect_success 'tag -m implies annotated' '
	(
	cd repo &&
	grit tag -m "Release 2.5" v2.5 HEAD~1 &&
	grit cat-file -t v2.5 >actual &&
	echo "tag" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'annotated tag has correct type' '
	(
	cd repo &&
	grit cat-file -t v2.0 >actual &&
	echo "tag" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'lightweight tag is a commit' '
	(
	cd repo &&
	grit cat-file -t v1.0 >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

# -- listing -----------------------------------------------------------------

test_expect_success 'tag -l lists all tags' '
	(
	cd repo &&
	grit tag -l >actual &&
	grep "v1.0" actual &&
	grep "v2.0" actual &&
	grep "v2.5" actual &&
	grep "v3.0" actual
	)
'

test_expect_success 'tag -l with pattern' '
	(
	cd repo &&
	grit tag -l "v2*" >actual &&
	grep "v2.0" actual &&
	grep "v2.5" actual &&
	! grep "v1.0" actual &&
	! grep "v3.0" actual
	)
'

test_expect_success 'tag -l with no match returns empty' '
	(
	cd repo &&
	grit tag -l "x*" >actual &&
	test_must_be_empty actual
	)
'

# -- sort --------------------------------------------------------------------

test_expect_success 'tag --sort=refname sorts alphabetically' '
	(
	cd repo &&
	grit tag --sort=refname -l >actual &&
	head -1 actual >first &&
	echo "v1.0" >expect &&
	test_cmp expect first
	)
'

test_expect_success 'tag --sort=version:refname sorts by version' '
	(
	cd repo &&
	grit tag --sort=version:refname -l >actual &&
	head -1 actual >first &&
	echo "v1.0" >expect &&
	test_cmp expect first &&
	tail -1 actual >last &&
	echo "v3.0" >expect_last &&
	test_cmp expect_last last
	)
'

test_expect_success 'tag --sort=-refname sorts reverse alphabetically' '
	(
	cd repo &&
	grit tag --sort=-refname -l >actual &&
	head -1 actual >first &&
	echo "v3.0" >expect &&
	test_cmp expect first
	)
'

test_expect_success 'tag --sort=-version:refname sorts reverse version' '
	(
	cd repo &&
	grit tag --sort=-version:refname -l >actual &&
	head -1 actual >first &&
	echo "v3.0" >expect &&
	test_cmp expect first &&
	tail -1 actual >last &&
	echo "v1.0" >expect_last &&
	test_cmp expect_last last
	)
'

# -- delete ------------------------------------------------------------------

test_expect_success 'tag -d deletes a tag' '
	(
	cd repo &&
	grit tag delete-me &&
	grit tag -l >before &&
	grep "delete-me" before &&
	grit tag -d delete-me &&
	grit tag -l >after &&
	! grep "delete-me" after
	)
'

test_expect_success 'tag -d nonexistent tag fails' '
	(
	cd repo &&
	! grit tag -d no-such-tag 2>err &&
	test -s err
	)
'

# -- force -------------------------------------------------------------------

test_expect_success 'tag refuses to overwrite without --force' '
	(
	cd repo &&
	! grit tag v1.0 HEAD 2>err
	)
'

test_expect_success 'tag --force overwrites existing tag' '
	(
	cd repo &&
	grit tag --force v1.0 HEAD &&
	grit rev-parse v1.0 >actual &&
	grit rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

# -- contains ----------------------------------------------------------------

test_expect_success 'tag --contains shows tags containing commit' '
	(
	cd repo &&
	grit tag --contains HEAD >actual &&
	grep "v3.0" actual
	)
'

test_expect_success 'tag --contains HEAD~2 shows all relevant tags' '
	(
	cd repo &&
	grit tag --contains HEAD~2 >actual &&
	grep "v3.0" actual
	)
'

# -- case insensitive --------------------------------------------------------

test_expect_success 'tag -i -l sorts case insensitively' '
	(
	cd repo &&
	grit tag Alpha &&
	grit tag beta &&
	grit tag -i --sort=refname -l >actual &&
	grep "Alpha" actual &&
	grep "beta" actual
	)
'

# -- comparison with real git ------------------------------------------------

test_expect_success 'setup: comparison repos' '
	(
	$REAL_GIT init git-cmp &&
	cd git-cmp &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "x" >f.txt &&
	$REAL_GIT add f.txt &&
	test_tick &&
	$REAL_GIT commit -m "init" &&
	$REAL_GIT tag cmp-tag &&
	cd .. &&
	grit init grit-cmp &&
	cd grit-cmp &&
	echo "x" >f.txt &&
	grit add f.txt &&
	test_tick &&
	grit commit -m "init" &&
	grit tag cmp-tag &&
	cd ..
	)
'

test_expect_success 'tag list matches real git' '
	$REAL_GIT -C git-cmp tag -l >expect &&
	grit -C grit-cmp tag -l >actual &&
	test_cmp expect actual
'

test_expect_success 'tag -d output: tag is gone' '
	$REAL_GIT -C git-cmp tag -d cmp-tag &&
	grit -C grit-cmp tag -d cmp-tag &&
	$REAL_GIT -C git-cmp tag -l >expect &&
	grit -C grit-cmp tag -l >actual &&
	test_cmp expect actual
'

test_expect_success 'annotated tag -n1 shows message' '
	(
	cd repo &&
	grit tag -n1 -l >actual &&
	grep "Release 2.0" actual
	)
'

test_done
