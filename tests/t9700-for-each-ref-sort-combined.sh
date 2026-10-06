#!/bin/sh
# Tests for grit for-each-ref with --sort, --format, --count, and pattern filtering.

test_description='grit for-each-ref sort, format, count, and pattern combinations'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with branches and tags' '
	(
	grit init --initial-branch=master repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo alpha >alpha.txt &&
	grit add . &&
	grit commit -m "first" &&
	grit branch feature-a &&
	grit branch feature-b &&
	echo beta >beta.txt &&
	grit add . &&
	grit commit -m "second" &&
	grit tag v1.0 &&
	grit tag -a v2.0 -m "annotated v2.0" &&
	grit branch release/1.0 &&
	echo gamma >gamma.txt &&
	grit add . &&
	grit commit -m "third" &&
	grit tag v3.0 &&
	grit tag -a v4.0 -m "annotated v4.0"
	)
'

###########################################################################
# Section 2: Basic for-each-ref
###########################################################################

test_expect_success 'for-each-ref lists all refs' '
	(
	cd repo &&
	grit for-each-ref >actual &&
	test $(wc -l <actual) -ge 7
	)
'

test_expect_success 'for-each-ref output has three columns by default' '
	(
	cd repo &&
	grit for-each-ref >actual &&
	while IFS= read -r line; do
		set -- $line &&
		echo "$1" | grep -qE "^[0-9a-f]{40}$" || return 1
	done <actual
	)
'

test_expect_success 'for-each-ref default output includes commit and tag types' '
	(
	cd repo &&
	grit for-each-ref >actual &&
	grep "commit" actual &&
	grep "tag" actual
	)
'

###########################################################################
# Section 3: Pattern filtering
###########################################################################

test_expect_success 'for-each-ref refs/heads shows only branches' '
	(
	cd repo &&
	grit for-each-ref refs/heads >actual &&
	grep "refs/heads/" actual &&
	! grep "refs/tags/" actual
	)
'

test_expect_success 'for-each-ref refs/tags shows only tags' '
	(
	cd repo &&
	grit for-each-ref refs/tags >actual &&
	grep "refs/tags/" actual &&
	! grep "refs/heads/" actual
	)
'

test_expect_success 'for-each-ref with specific branch pattern' '
	(
	cd repo &&
	grit for-each-ref refs/heads/master >actual &&
	test $(wc -l <actual) -eq 1 &&
	grep "refs/heads/master" actual
	)
'

test_expect_success 'for-each-ref with release pattern' '
	(
	cd repo &&
	grit for-each-ref refs/heads/release >actual &&
	grep "refs/heads/release" actual
	)
'

test_expect_success 'for-each-ref with non-matching pattern returns empty' '
	(
	cd repo &&
	grit for-each-ref refs/heads/nonexistent >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 4: --format option
###########################################################################

test_expect_success 'for-each-ref --format=%(refname) shows only ref names' '
	(
	cd repo &&
	grit for-each-ref "--format=%(refname)" >actual &&
	grep "refs/heads/master" actual &&
	grep "refs/tags/v1.0" actual &&
	! grep "commit" actual
	)
'

test_expect_success 'for-each-ref --format=%(objectname) shows only OIDs' '
	(
	cd repo &&
	grit for-each-ref "--format=%(objectname)" >actual &&
	while IFS= read -r line; do
		echo "$line" | grep -qE "^[0-9a-f]{40}$" || return 1
	done <actual
	)
'

test_expect_success 'for-each-ref --format=%(objecttype) shows types' '
	(
	cd repo &&
	grit for-each-ref "--format=%(objecttype)" >actual &&
	grep "commit" actual &&
	grep "tag" actual
	)
'

test_expect_success 'for-each-ref format with multiple placeholders' '
	(
	cd repo &&
	grit for-each-ref "--format=%(objectname) %(refname)" >actual &&
	head -1 actual | grep -qE "^[0-9a-f]{40} refs/" || return 1
	)
'

test_expect_success 'for-each-ref format with literal text' '
	(
	cd repo &&
	grit for-each-ref "--format=REF:%(refname)" >actual &&
	grep "^REF:refs/" actual
	)
'

test_expect_success 'for-each-ref format with pattern filter' '
	(
	cd repo &&
	grit for-each-ref "--format=%(refname)" refs/tags >actual &&
	grep "refs/tags/v1.0" actual &&
	! grep "refs/heads/" actual
	)
'

###########################################################################
# Section 5: --sort option
###########################################################################

test_expect_success 'for-each-ref --sort=refname orders alphabetically' '
	(
	cd repo &&
	grit for-each-ref --sort=refname "--format=%(refname)" >actual &&
	sort actual >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref --sort=objectname orders by hash' '
	(
	cd repo &&
	grit for-each-ref --sort=objectname "--format=%(objectname)" >actual &&
	sort actual >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref --sort=objecttype groups by type' '
	(
	cd repo &&
	grit for-each-ref --sort=objecttype "--format=%(objecttype)" >actual &&
	sort actual >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref sort with pattern filter' '
	(
	cd repo &&
	grit for-each-ref --sort=refname "--format=%(refname)" refs/heads >actual &&
	sort actual >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref sort=refname on tags only' '
	(
	cd repo &&
	grit for-each-ref --sort=refname "--format=%(refname)" refs/tags >actual &&
	sort actual >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 6: --count option
###########################################################################

test_expect_success 'for-each-ref --count=1 returns one ref' '
	(
	cd repo &&
	grit for-each-ref --count=1 >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'for-each-ref --count=3 returns three refs' '
	(
	cd repo &&
	grit for-each-ref --count=3 >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'for-each-ref --count=0 returns empty' '
	(
	cd repo &&
	grit for-each-ref --count=0 >count0 &&
	test_must_be_empty count0
	)
'

test_expect_success 'for-each-ref --count larger than total returns all' '
	(
	cd repo &&
	grit for-each-ref >all &&
	total=$(wc -l <all) &&
	grit for-each-ref --count=999 >big &&
	test $(wc -l <big) -eq $total
	)
'

###########################################################################
# Section 7: Combined options
###########################################################################

test_expect_success 'for-each-ref sort + format combined' '
	(
	cd repo &&
	grit for-each-ref --sort=refname "--format=%(refname)" >actual &&
	head -1 actual | grep "refs/heads/feature-a"
	)
'

test_expect_success 'for-each-ref sort + count combined' '
	(
	cd repo &&
	grit for-each-ref --sort=refname --count=2 "--format=%(refname)" >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'for-each-ref sort + format + count combined' '
	(
	cd repo &&
	grit for-each-ref --sort=refname --count=1 "--format=%(refname)" >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'for-each-ref format + pattern combined' '
	(
	cd repo &&
	grit for-each-ref "--format=%(objectname) %(refname)" refs/tags >actual &&
	lines=$(wc -l <actual) &&
	test $lines -ge 4 &&
	! grep "refs/heads" actual
	)
'

test_expect_success 'for-each-ref all three: sort + count + pattern' '
	(
	cd repo &&
	grit for-each-ref --sort=refname --count=2 refs/heads >actual &&
	test $(wc -l <actual) -eq 2
	)
'

###########################################################################
# Section 8: Cross-check with real git
###########################################################################

test_expect_success 'setup cross-check repo' '
	(
	$REAL_GIT init --initial-branch=master cross &&
	cd cross &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo one >one.txt &&
	$REAL_GIT add . &&
	$REAL_GIT commit -m "one" &&
	$REAL_GIT branch br-alpha &&
	$REAL_GIT branch br-beta &&
	$REAL_GIT tag t1
	)
'

test_expect_success 'for-each-ref output matches real git' '
	(
	cd cross &&
	grit for-each-ref >grit_out &&
	$REAL_GIT for-each-ref >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'for-each-ref --format=%(refname) matches real git' '
	(
	cd cross &&
	grit for-each-ref "--format=%(refname)" >grit_out &&
	$REAL_GIT for-each-ref "--format=%(refname)" >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'for-each-ref --sort=refname matches real git' '
	(
	cd cross &&
	grit for-each-ref --sort=refname >grit_out &&
	$REAL_GIT for-each-ref --sort=refname >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'for-each-ref --count=2 matches real git' '
	(
	cd cross &&
	grit for-each-ref --count=2 >grit_out &&
	$REAL_GIT for-each-ref --count=2 >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'for-each-ref refs/heads matches real git' '
	(
	cd cross &&
	grit for-each-ref refs/heads >grit_out &&
	$REAL_GIT for-each-ref refs/heads >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'for-each-ref refs/tags matches real git' '
	(
	cd cross &&
	grit for-each-ref refs/tags >grit_out &&
	$REAL_GIT for-each-ref refs/tags >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'for-each-ref sort + format matches real git' '
	(
	cd cross &&
	grit for-each-ref --sort=refname "--format=%(refname)" >grit_out &&
	$REAL_GIT for-each-ref --sort=refname "--format=%(refname)" >git_out &&
	test_cmp grit_out git_out
	)
'

test_done
