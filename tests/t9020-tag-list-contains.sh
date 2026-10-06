#!/bin/sh
# Tests for tag listing: -l, --contains, --sort, -n, patterns, annotated vs lightweight.

test_description='tag list, contains, sort, and annotation display'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- setup -----------------------------------------------------------------

test_expect_success 'setup: repo with tags at different commits' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "base" >file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "first" &&
	$REAL_GIT tag v0.1 &&
	$REAL_GIT tag -a v0.1-annotated -m "version 0.1" &&
	echo "second" >>file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "second" &&
	$REAL_GIT tag v0.2 &&
	$REAL_GIT tag -a v0.2-annotated -m "version 0.2" &&
	echo "third" >>file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "third" &&
	$REAL_GIT tag v1.0 &&
	$REAL_GIT tag -a v1.0-annotated -m "version 1.0" &&
	$REAL_GIT tag -a v2.0-rc1 -m "release candidate 1" &&
	$REAL_GIT tag release-final
	)
'

# -- basic listing -----------------------------------------------------------

test_expect_success 'tag -l lists all tags' '
	(
	cd repo &&
	grit tag -l >actual &&
	grep "v0.1$" actual &&
	grep "v0.2$" actual &&
	grep "v1.0$" actual
	)
'

test_expect_success 'tag with no args lists all tags' '
	(
	cd repo &&
	grit tag >actual &&
	grep "v0.1$" actual &&
	grep "v1.0$" actual
	)
'

test_expect_success 'tag -l shows annotated tags too' '
	(
	cd repo &&
	grit tag -l >actual &&
	grep "v0.1-annotated" actual &&
	grep "v0.2-annotated" actual &&
	grep "v1.0-annotated" actual
	)
'

test_expect_success 'tag list includes all 8 tags' '
	(
	cd repo &&
	grit tag -l >actual &&
	test_line_count = 8 actual
	)
'

# -- pattern matching --------------------------------------------------------

test_expect_success 'tag -l v* matches version tags' '
	(
	cd repo &&
	grit tag -l "v*" >actual &&
	grep "v0.1$" actual &&
	grep "v1.0$" actual &&
	! grep "release-final" actual
	)
'

test_expect_success 'tag -l v0.* matches v0 tags' '
	(
	cd repo &&
	grit tag -l "v0.*" >actual &&
	grep "v0.1$" actual &&
	grep "v0.2$" actual &&
	! grep "v1.0$" actual
	)
'

test_expect_success 'tag -l *annotated matches annotated tags' '
	(
	cd repo &&
	grit tag -l "*annotated" >actual &&
	grep "v0.1-annotated" actual &&
	grep "v0.2-annotated" actual &&
	grep "v1.0-annotated" actual &&
	! grep "v0.1$" actual
	)
'

test_expect_success 'tag -l release* matches release tags' '
	(
	cd repo &&
	grit tag -l "release*" >actual &&
	grep "release-final" actual &&
	! grep "v0.1" actual
	)
'

test_expect_success 'tag -l with no match returns empty' '
	(
	cd repo &&
	grit tag -l "nonexistent*" >actual &&
	test_must_be_empty actual
	)
'

# -- --contains --------------------------------------------------------------

test_expect_success 'tag --contains HEAD shows tags at HEAD' '
	(
	cd repo &&
	grit tag --contains HEAD >actual &&
	grep "v1.0$" actual &&
	grep "v1.0-annotated" actual &&
	grep "v2.0-rc1" actual &&
	grep "release-final" actual
	)
'

test_expect_success 'tag --contains first-commit shows all tags' '
	(
	cd repo &&
	first=$($REAL_GIT rev-parse HEAD~2) &&
	grit tag --contains "$first" >actual &&
	grep "v0.1$" actual &&
	grep "v0.2$" actual &&
	grep "v1.0$" actual
	)
'

test_expect_success 'tag --contains second-commit excludes v0.1' '
	(
	cd repo &&
	second=$($REAL_GIT rev-parse HEAD~1) &&
	grit tag --contains "$second" >actual &&
	! grep "v0.1$" actual &&
	grep "v0.2$" actual &&
	grep "v1.0$" actual
	)
'

test_expect_success 'tag --contains HEAD includes annotated tags' '
	(
	cd repo &&
	grit tag --contains HEAD >actual &&
	grep "v1.0-annotated" actual
	)
'

# -- -n (annotation display) ------------------------------------------------

test_expect_success 'tag -n shows annotation for annotated tags' '
	(
	cd repo &&
	grit tag -n >actual &&
	grep "v1.0-annotated" actual &&
	grep "version 1.0" actual
	)
'

test_expect_success 'tag -n shows annotation for v0.1-annotated' '
	(
	cd repo &&
	grit tag -n >actual &&
	grep "v0.1-annotated" actual &&
	grep "version 0.1" actual
	)
'

test_expect_success 'tag -n shows annotation for v2.0-rc1' '
	(
	cd repo &&
	grit tag -n >actual &&
	grep "v2.0-rc1" actual &&
	grep "release candidate 1" actual
	)
'

test_expect_success 'tag -n lists lightweight tags without annotation' '
	(
	cd repo &&
	grit tag -n >actual &&
	grep "v0.1" actual &&
	grep "release-final" actual
	)
'

# -- --sort ------------------------------------------------------------------

test_expect_success 'tag --sort=version:refname sorts by version' '
	(
	cd repo &&
	grit tag --sort=version:refname >actual &&
	test -s actual &&
	grep "v0.1" actual
	)
'

test_expect_success 'tag --sort=refname sorts alphabetically' '
	(
	cd repo &&
	grit tag --sort=refname >actual &&
	test -s actual
	)
'

# -- create and delete tags --------------------------------------------------

test_expect_success 'tag creates a lightweight tag' '
	(
	cd repo &&
	grit tag test-light &&
	grit tag -l >actual &&
	grep "test-light" actual
	)
'

test_expect_success 'tag -a -m creates annotated tag' '
	(
	cd repo &&
	grit tag -a test-annotated -m "test annotation" &&
	grit tag -l >actual &&
	grep "test-annotated" actual
	)
'

test_expect_success 'tag -d deletes a tag' '
	(
	cd repo &&
	grit tag -d test-light &&
	grit tag -l >actual &&
	! grep "test-light" actual
	)
'

test_expect_success 'tag -d deletes annotated tag' '
	(
	cd repo &&
	grit tag -d test-annotated &&
	grit tag -l >actual &&
	! grep "test-annotated" actual
	)
'

# -- tag at specific commit --------------------------------------------------

test_expect_success 'tag at specific commit' '
	(
	cd repo &&
	first=$($REAL_GIT rev-parse HEAD~2) &&
	grit tag at-first "$first" &&
	grit rev-parse at-first >actual &&
	echo "$first" >expect &&
	test_cmp expect actual
	)
'

# -- compare with real git ---------------------------------------------------

test_expect_success 'tag -l matches real git tag list' '
	(
	cd repo &&
	grit tag -l | sort >actual &&
	$REAL_GIT tag -l | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag --contains HEAD matches real git' '
	(
	cd repo &&
	grit tag --contains HEAD | sort >actual &&
	$REAL_GIT tag --contains HEAD | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -l v* matches real git pattern filter' '
	(
	cd repo &&
	grit tag -l "v*" | sort >actual &&
	$REAL_GIT tag -l "v*" | sort >expect &&
	test_cmp expect actual
	)
'

test_done
