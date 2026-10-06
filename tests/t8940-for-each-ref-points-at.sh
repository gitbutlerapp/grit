#!/bin/sh
# Tests for for-each-ref --points-at, --contains, --merged, pattern matching, and formatting.

test_description='for-each-ref --points-at and related filters'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- setup -----------------------------------------------------------------

test_expect_success 'setup: repo with branches and tags' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "base" >file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "first" &&
	$REAL_GIT tag v1.0 &&
	$REAL_GIT tag -a v1.0-annotated -m "annotated v1.0" &&
	echo "second" >>file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "second" &&
	$REAL_GIT tag v2.0 &&
	$REAL_GIT branch feature &&
	echo "third" >>file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "third" &&
	$REAL_GIT tag v3.0 &&
	$REAL_GIT branch bugfix HEAD~1
	)
'

# -- basic listing ----------------------------------------------------------

test_expect_success 'for-each-ref lists all refs' '
	(
	cd repo &&
	grit for-each-ref >actual &&
	test $(wc -l <actual) -gt 0
	)
'

test_expect_success 'for-each-ref refs/heads/ lists only branches' '
	(
	cd repo &&
	grit for-each-ref refs/heads/ >actual &&
	! grep refs/tags/ actual &&
	grep refs/heads/ actual
	)
'

test_expect_success 'for-each-ref refs/tags/ lists only tags' '
	(
	cd repo &&
	grit for-each-ref refs/tags/ >actual &&
	! grep refs/heads/ actual &&
	grep refs/tags/ actual
	)
'

test_expect_success 'for-each-ref with no matching pattern gives empty' '
	(
	cd repo &&
	grit for-each-ref refs/remotes/ >actual &&
	test $(wc -l <actual) = 0
	)
'

# -- --format ---------------------------------------------------------------

test_expect_success 'for-each-ref --format=%(refname)' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/ | sort >actual &&
	printf "refs/heads/bugfix\nrefs/heads/feature\nrefs/heads/master\n" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref --format=%(refname:short) refs/heads/' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" refs/heads/ | sort >actual &&
	printf "bugfix\nfeature\nmaster\n" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref --format=%(objectname) outputs SHAs' '
	(
	cd repo &&
	grit for-each-ref --format="%(objectname)" refs/heads/master >actual &&
	grit rev-parse refs/heads/master >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref --format=%(objecttype) shows commit' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype)" refs/heads/master >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref --format with multiple atoms' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype) %(refname)" refs/heads/master >actual &&
	sha=$(grit rev-parse refs/heads/master) &&
	echo "commit refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

# -- --points-at ------------------------------------------------------------

test_expect_success 'for-each-ref --points-at HEAD shows HEAD refs' '
	(
	cd repo &&
	grit for-each-ref --points-at=HEAD --format="%(refname)" | sort >actual &&
	head_sha=$(grit rev-parse HEAD) &&
	# master and v3.0 point at HEAD
	grep refs/heads/master actual &&
	grep refs/tags/v3.0 actual
	)
'

test_expect_success 'for-each-ref --points-at HEAD does not include feature' '
	(
	cd repo &&
	grit for-each-ref --points-at=HEAD --format="%(refname)" >actual &&
	! grep refs/heads/feature actual
	)
'

test_expect_success 'for-each-ref --points-at with SHA' '
	(
	cd repo &&
	sha=$(grit rev-parse v2.0) &&
	grit for-each-ref --points-at="$sha" --format="%(refname)" | sort >actual &&
	grep refs/tags/v2.0 actual &&
	grep refs/heads/feature actual
	)
'

test_expect_success 'for-each-ref --points-at v1.0 does not include v3.0' '
	(
	cd repo &&
	grit for-each-ref --points-at=v1.0 --format="%(refname)" >actual &&
	! grep refs/tags/v3.0 actual &&
	! grep refs/heads/master actual
	)
'

test_expect_success 'for-each-ref --points-at with pattern restricts namespace' '
	(
	cd repo &&
	sha=$(grit rev-parse v2.0) &&
	grit for-each-ref --points-at="$sha" --format="%(refname)" refs/tags/ >actual &&
	grep refs/tags/v2.0 actual &&
	! grep refs/heads/ actual
	)
'

# -- --contains -------------------------------------------------------------

test_expect_success 'for-each-ref --contains HEAD shows refs at or after HEAD' '
	(
	cd repo &&
	grit for-each-ref --contains=HEAD --format="%(refname)" | sort >actual &&
	grep refs/heads/master actual
	)
'

test_expect_success 'for-each-ref --contains excludes refs before target' '
	(
	cd repo &&
	grit for-each-ref --contains=HEAD --format="%(refname)" >actual &&
	! grep refs/tags/v1.0 actual &&
	! grep refs/heads/bugfix actual
	)
'

test_expect_success 'for-each-ref --contains root includes all branches' '
	(
	cd repo &&
	root=$(grit rev-list --reverse HEAD | head -1) &&
	grit for-each-ref --contains="$root" --format="%(refname)" refs/heads/ | sort >actual &&
	printf "refs/heads/bugfix\nrefs/heads/feature\nrefs/heads/master\n" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref --contains with tag name' '
	(
	cd repo &&
	grit for-each-ref --contains=v2.0 --format="%(refname:short)" refs/heads/ | sort >actual &&
	grep master actual &&
	grep feature actual
	)
'

# -- --merged and --no-merged -----------------------------------------------

test_expect_success 'for-each-ref --merged=HEAD shows merged branches' '
	(
	cd repo &&
	grit for-each-ref --merged=HEAD --format="%(refname:short)" refs/heads/ | sort >actual &&
	grep master actual &&
	grep bugfix actual &&
	grep feature actual
	)
'

test_expect_success 'for-each-ref --merged=v2.0 excludes master' '
	(
	cd repo &&
	grit for-each-ref --merged=v2.0 --format="%(refname:short)" refs/heads/ | sort >actual &&
	grep bugfix actual &&
	grep feature actual &&
	! grep master actual
	)
'

test_expect_success 'for-each-ref --no-merged=HEAD shows nothing (all merged)' '
	(
	cd repo &&
	grit for-each-ref --no-merged=HEAD --format="%(refname:short)" refs/heads/ >actual &&
	test $(wc -l <actual) = 0
	)
'

test_expect_success 'for-each-ref --no-merged=v1.0 shows branches ahead of v1.0' '
	(
	cd repo &&
	grit for-each-ref --no-merged=v1.0 --format="%(refname:short)" refs/heads/ | sort >actual &&
	grep master actual
	)
'

# -- --sort -----------------------------------------------------------------

test_expect_success 'for-each-ref --sort=refname orders alphabetically' '
	(
	cd repo &&
	grit for-each-ref --sort=refname --format="%(refname:short)" refs/tags/ >actual &&
	sort -c actual
	)
'

test_expect_success 'for-each-ref --sort=-refname reverses order' '
	(
	cd repo &&
	grit for-each-ref --sort=-refname --format="%(refname:short)" refs/tags/ >actual &&
	first=$(head -1 actual) &&
	last=$(tail -1 actual) &&
	test "$first" ">" "$last"
	)
'

# -- --count ----------------------------------------------------------------

test_expect_success 'for-each-ref --count=1 shows single ref' '
	(
	cd repo &&
	grit for-each-ref --count=1 >actual &&
	test $(wc -l <actual) = 1
	)
'

test_expect_success 'for-each-ref --count=2 shows two refs' '
	(
	cd repo &&
	grit for-each-ref --count=2 >actual &&
	test $(wc -l <actual) = 2
	)
'

# -- annotated tags ---------------------------------------------------------

test_expect_success 'for-each-ref shows annotated tag as tag object type' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype)" refs/tags/v1.0-annotated >actual &&
	echo "tag" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref shows lightweight tag as commit type' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype)" refs/tags/v1.0 >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_done
