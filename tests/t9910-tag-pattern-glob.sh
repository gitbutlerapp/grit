#!/bin/sh
# Tests for grit tag -l with glob patterns, sorting, and filtering.

test_description='grit tag --list pattern matching and globbing'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with many tags' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "v1" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "release v1.0" &&
	"$REAL_GIT" tag v1.0 &&
	"$REAL_GIT" tag v1.0.1 &&
	"$REAL_GIT" tag v1.1 &&
	"$REAL_GIT" tag v1.1.1 &&
	echo "v2" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "release v2.0" &&
	"$REAL_GIT" tag v2.0 &&
	"$REAL_GIT" tag v2.0-rc1 &&
	"$REAL_GIT" tag v2.0-rc2 &&
	"$REAL_GIT" tag v2.1 &&
	"$REAL_GIT" tag release-1.0 &&
	"$REAL_GIT" tag release-2.0 &&
	"$REAL_GIT" tag alpha &&
	"$REAL_GIT" tag beta &&
	"$REAL_GIT" tag -a annotated-v1 -m "Annotated tag v1" HEAD~1 &&
	"$REAL_GIT" tag -a annotated-v2 -m "Annotated tag v2" HEAD
	)
'

###########################################################################
# Section 2: tag -l (list all)
###########################################################################

test_expect_success 'tag -l lists all tags' '
	(
	cd repo &&
	grit tag -l >actual &&
	test_line_count -ge 12 actual
	)
'

test_expect_success 'tag -l matches real git' '
	(
	cd repo &&
	grit tag -l >actual &&
	"$REAL_GIT" tag -l >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag with no args lists all tags' '
	(
	cd repo &&
	grit tag >actual &&
	grit tag -l >list_out &&
	test_cmp list_out actual
	)
'

###########################################################################
# Section 3: Simple glob patterns
###########################################################################

test_expect_success 'tag -l "v*" lists only v-prefixed tags' '
	(
	cd repo &&
	grit tag -l "v*" >actual &&
	while read tag; do
		case "$tag" in v*) ;; *) echo "unexpected: $tag"; return 1;; esac
	done <actual &&
	test_line_count -ge 7 actual
	)
'

test_expect_success 'tag -l "v*" matches real git' '
	(
	cd repo &&
	grit tag -l "v*" >actual &&
	"$REAL_GIT" tag -l "v*" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -l "v1.*" lists v1 tags only' '
	(
	cd repo &&
	grit tag -l "v1.*" >actual &&
	grep "v1.0" actual &&
	grep "v1.1" actual &&
	! grep "v2" actual
	)
'

test_expect_success 'tag -l "v1.*" matches real git' '
	(
	cd repo &&
	grit tag -l "v1.*" >actual &&
	"$REAL_GIT" tag -l "v1.*" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -l "v2.0*" includes rc tags' '
	(
	cd repo &&
	grit tag -l "v2.0*" >actual &&
	grep "v2.0" actual &&
	grep "v2.0-rc1" actual &&
	grep "v2.0-rc2" actual
	)
'

test_expect_success 'tag -l "v2.0*" matches real git' '
	(
	cd repo &&
	grit tag -l "v2.0*" >actual &&
	"$REAL_GIT" tag -l "v2.0*" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 4: More complex patterns
###########################################################################

test_expect_success 'tag -l "release-*" lists release tags' '
	(
	cd repo &&
	grit tag -l "release-*" >actual &&
	grep "release-1.0" actual &&
	grep "release-2.0" actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'tag -l "release-*" matches real git' '
	(
	cd repo &&
	grit tag -l "release-*" >actual &&
	"$REAL_GIT" tag -l "release-*" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -l "*-rc*" lists release candidates' '
	(
	cd repo &&
	grit tag -l "*-rc*" >actual &&
	grep "v2.0-rc1" actual &&
	grep "v2.0-rc2" actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'tag -l "*-rc*" matches real git' '
	(
	cd repo &&
	grit tag -l "*-rc*" >actual &&
	"$REAL_GIT" tag -l "*-rc*" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -l "a*" lists alpha and annotated tags' '
	(
	cd repo &&
	grit tag -l "a*" >actual &&
	grep "alpha" actual &&
	grep "annotated-v1" actual &&
	grep "annotated-v2" actual
	)
'

test_expect_success 'tag -l "a*" matches real git' '
	(
	cd repo &&
	grit tag -l "a*" >actual &&
	"$REAL_GIT" tag -l "a*" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 5: Pattern that matches nothing
###########################################################################

test_expect_success 'tag -l with non-matching pattern produces empty output' '
	(
	cd repo &&
	grit tag -l "zzz*" >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'tag -l non-matching exit code matches real git' '
	(
	cd repo &&
	grit tag -l "zzz*" >actual; grit_rc=$? &&
	"$REAL_GIT" tag -l "zzz*" >expect; git_rc=$? &&
	test "$grit_rc" = "$git_rc"
	)
'

###########################################################################
# Section 6: Exact tag name as pattern
###########################################################################

test_expect_success 'tag -l with exact name lists one tag' '
	(
	cd repo &&
	grit tag -l "v1.0" >actual &&
	test_line_count = 1 actual &&
	grep "v1.0" actual
	)
'

test_expect_success 'tag -l exact name matches real git' '
	(
	cd repo &&
	grit tag -l "v1.0" >actual &&
	"$REAL_GIT" tag -l "v1.0" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 7: --contains with pattern
###########################################################################

test_expect_success 'tag --contains HEAD lists tags on HEAD' '
	(
	cd repo &&
	grit tag --contains HEAD >actual &&
	grep "v2.0" actual
	)
'

test_expect_success 'tag --contains matches real git' '
	(
	cd repo &&
	grit tag --contains HEAD >actual &&
	"$REAL_GIT" tag --contains HEAD >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 8: --sort
###########################################################################

test_expect_success 'tag -l --sort=refname is alphabetical' '
	(
	cd repo &&
	grit tag -l --sort=refname >actual &&
	sort >sorted <actual &&
	test_cmp sorted actual
	)
'

test_expect_success 'tag -l --sort=refname matches real git' '
	(
	cd repo &&
	grit tag -l --sort=refname >actual &&
	"$REAL_GIT" tag -l --sort=refname >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -l --sort=-refname is reverse alphabetical' '
	(
	cd repo &&
	grit tag -l --sort=-refname >actual &&
	sort -r >sorted <actual &&
	test_cmp sorted actual
	)
'

test_expect_success 'tag -l --sort=-refname matches real git' '
	(
	cd repo &&
	grit tag -l --sort=-refname >actual &&
	"$REAL_GIT" tag -l --sort=-refname >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 9: -n with annotated tags
###########################################################################

test_expect_success 'tag -l -n1 shows annotation for annotated tags' '
	(
	cd repo &&
	grit tag -l -n1 "annotated-*" >actual &&
	grep "Annotated tag v1" actual &&
	grep "Annotated tag v2" actual
	)
'

test_expect_success 'tag -l -n1 matches real git' '
	(
	cd repo &&
	grit tag -l -n1 "annotated-*" >actual &&
	"$REAL_GIT" tag -l -n "annotated-*" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 10: Deletion and re-check
###########################################################################

test_expect_success 'tag -d removes tag from listing' '
	(
	cd repo &&
	grit tag temp-tag &&
	grit tag -l >before &&
	grep "temp-tag" before &&
	grit tag -d temp-tag &&
	grit tag -l >after &&
	! grep "temp-tag" after
	)
'

test_expect_success 'tag -d then -l matches real git' '
	(
	cd repo &&
	grit tag to-delete &&
	grit tag -d to-delete &&
	grit tag -l >actual &&
	"$REAL_GIT" tag -l >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -l "v?.0" question mark glob' '
	(
	cd repo &&
	grit tag -l "v?.0" >actual &&
	grep "v1.0" actual &&
	grep "v2.0" actual &&
	! grep "v1.0.1" actual
	)
'

test_expect_success 'tag -l "v?.0" matches real git' '
	(
	cd repo &&
	grit tag -l "v?.0" >actual &&
	"$REAL_GIT" tag -l "v?.0" >expect &&
	test_cmp expect actual
	)
'

test_done
