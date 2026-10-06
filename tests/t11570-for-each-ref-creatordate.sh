#!/bin/sh
# Tests for grit for-each-ref with format atoms, sorting, count, and patterns.

test_description='grit for-each-ref: format atoms, sorting, count, patterns'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with branches and tags' '
	(
	"$REAL_GIT" init -b main repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&

	"$REAL_GIT" commit --allow-empty -m "first commit" &&
	"$REAL_GIT" branch alpha &&

	"$REAL_GIT" commit --allow-empty -m "second commit" &&
	"$REAL_GIT" branch beta &&

	"$REAL_GIT" commit --allow-empty -m "third commit" &&
	"$REAL_GIT" branch gamma &&

	"$REAL_GIT" commit --allow-empty -m "fourth commit" &&

	"$REAL_GIT" tag v1.0 alpha &&
	"$REAL_GIT" tag -a -m "annotated v2.0" v2.0 beta &&
	"$REAL_GIT" tag -a -m "annotated v3.0" v3.0 gamma &&
	"$REAL_GIT" tag v4.0-lw main
	)
'

###########################################################################
# Section 2: Basic for-each-ref listing
###########################################################################

test_expect_success 'for-each-ref: lists all refs' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref >actual &&
	"$REAL_GIT" for-each-ref >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: refs/heads pattern' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref refs/heads >actual &&
	"$REAL_GIT" for-each-ref refs/heads >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: refs/tags pattern' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref refs/tags >actual &&
	"$REAL_GIT" for-each-ref refs/tags >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: single branch pattern' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref refs/heads/alpha >actual &&
	"$REAL_GIT" for-each-ref refs/heads/alpha >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: single tag pattern' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref refs/tags/v1.0 >actual &&
	"$REAL_GIT" for-each-ref refs/tags/v1.0 >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: nonexistent pattern yields empty' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref refs/nonexistent >actual &&
	test ! -s actual
	)
'

###########################################################################
# Section 3: Format with refname
###########################################################################

test_expect_success 'for-each-ref: format %(refname)' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(refname)" >actual &&
	"$REAL_GIT" for-each-ref --format="%(refname)" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: format %(refname:short)' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(refname:short)" >actual &&
	"$REAL_GIT" for-each-ref --format="%(refname:short)" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: format %(refname) heads only' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(refname)" refs/heads >actual &&
	"$REAL_GIT" for-each-ref --format="%(refname)" refs/heads >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: format %(refname:short) heads only' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(refname:short)" refs/heads >actual &&
	"$REAL_GIT" for-each-ref --format="%(refname:short)" refs/heads >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 4: Format with objectname and objecttype
###########################################################################

test_expect_success 'for-each-ref: format %(objectname)' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(objectname)" refs/heads >actual &&
	"$REAL_GIT" for-each-ref --format="%(objectname)" refs/heads >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: format %(objecttype)' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(objecttype)" refs/heads >actual &&
	"$REAL_GIT" for-each-ref --format="%(objecttype)" refs/heads >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: branches are all commit type' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(objecttype)" refs/heads >actual &&
	! grep -v commit actual
	)
'

test_expect_success 'for-each-ref: annotated tags are tag type' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(objecttype)" refs/tags/v2.0 >actual &&
	grep "tag" actual
	)
'

test_expect_success 'for-each-ref: lightweight tags are commit type' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(objecttype)" refs/tags/v1.0 >actual &&
	grep "commit" actual
	)
'

###########################################################################
# Section 5: Format with subject
###########################################################################

test_expect_success 'for-each-ref: format %(subject) on heads' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(subject)" refs/heads >actual &&
	"$REAL_GIT" for-each-ref --format="%(subject)" refs/heads >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: subject matches commit message' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(subject)" refs/heads/alpha >actual &&
	grep "first commit" actual
	)
'

test_expect_success 'for-each-ref: subject on annotated tag' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(subject)" refs/tags/v2.0 >actual &&
	"$REAL_GIT" for-each-ref --format="%(subject)" refs/tags/v2.0 >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 6: Sorting
###########################################################################

test_expect_success 'for-each-ref: --sort=refname (ascending)' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --sort=refname --format="%(refname:short)" refs/heads >actual &&
	"$REAL_GIT" for-each-ref --sort=refname --format="%(refname:short)" refs/heads >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: --sort=-refname (descending)' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --sort=-refname --format="%(refname:short)" refs/heads >actual &&
	"$REAL_GIT" for-each-ref --sort=-refname --format="%(refname:short)" refs/heads >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: --sort=objectname' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --sort=objectname --format="%(refname:short)" refs/heads >actual &&
	"$REAL_GIT" for-each-ref --sort=objectname --format="%(refname:short)" refs/heads >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: --sort=objecttype on tags' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --sort=objecttype --format="%(refname:short)" refs/tags >actual &&
	"$REAL_GIT" for-each-ref --sort=objecttype --format="%(refname:short)" refs/tags >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: default sort is refname' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(refname)" refs/heads >actual_default &&
	"$GUST_BIN" for-each-ref --sort=refname --format="%(refname)" refs/heads >actual_sorted &&
	test_cmp actual_sorted actual_default
	)
'

###########################################################################
# Section 7: --count
###########################################################################

test_expect_success 'for-each-ref: --count=2 limits output' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --count=2 --format="%(refname:short)" refs/heads >actual &&
	"$REAL_GIT" for-each-ref --count=2 --format="%(refname:short)" refs/heads >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: --count=1 single result' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --count=1 --format="%(refname:short)" refs/heads >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'for-each-ref: --count=0 returns nothing' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --count=0 --format="%(refname:short)" refs/heads >actual &&
	test ! -s actual
	)
'

test_expect_success 'for-each-ref: --count greater than total returns all' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --count=100 --format="%(refname:short)" refs/heads >actual &&
	"$GUST_BIN" for-each-ref --format="%(refname:short)" refs/heads >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 8: Multiple patterns
###########################################################################

test_expect_success 'for-each-ref: multiple patterns' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(refname)" refs/heads/alpha refs/heads/beta >actual &&
	"$REAL_GIT" for-each-ref --format="%(refname)" refs/heads/alpha refs/heads/beta >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: mixed head and tag patterns' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(refname)" refs/heads/alpha refs/tags/v1.0 >actual &&
	"$REAL_GIT" for-each-ref --format="%(refname)" refs/heads/alpha refs/tags/v1.0 >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 9: Combined format strings
###########################################################################

test_expect_success 'for-each-ref: multiple atoms in format' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(objectname) %(refname:short) %(subject)" refs/heads >actual &&
	"$REAL_GIT" for-each-ref --format="%(objectname) %(refname:short) %(subject)" refs/heads >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: format with literal text prefix' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="ref=%(refname:short)" refs/heads >actual &&
	"$REAL_GIT" for-each-ref --format="ref=%(refname:short)" refs/heads >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: format with separator between atoms' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(objecttype)|%(refname:short)" refs/heads >actual &&
	"$REAL_GIT" for-each-ref --format="%(objecttype)|%(refname:short)" refs/heads >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 10: Sort + count combined
###########################################################################

test_expect_success 'for-each-ref: --sort=-refname --count=2' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --sort=-refname --count=2 --format="%(refname:short)" refs/heads >actual &&
	"$REAL_GIT" for-each-ref --sort=-refname --count=2 --format="%(refname:short)" refs/heads >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'for-each-ref: --sort=objectname --count=1' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --sort=objectname --count=1 --format="%(refname:short)" refs/heads >actual &&
	"$REAL_GIT" for-each-ref --sort=objectname --count=1 --format="%(refname:short)" refs/heads >expected &&
	test_cmp expected actual
	)
'

test_done
