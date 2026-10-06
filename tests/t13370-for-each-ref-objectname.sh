#!/bin/sh
# Tests for grit for-each-ref focusing on %(objectname) and related format atoms.

test_description='grit for-each-ref objectname format atoms'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup: create repo with branches and tags' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "initial" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "first commit" &&
	"$REAL_GIT" branch alpha &&
	"$REAL_GIT" branch beta &&
	echo "second" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit" &&
	"$REAL_GIT" branch gamma &&
	"$REAL_GIT" tag -a v1.0 -m "version 1.0" &&
	"$REAL_GIT" tag lightweight-tag &&
	"$REAL_GIT" tag -a v2.0 -m "version 2.0"
	)
'

###########################################################################
# Basic %(objectname) tests
###########################################################################

test_expect_success 'for-each-ref %(objectname) shows full SHA for heads' '
	(cd repo && grit for-each-ref --format="%(objectname)" refs/heads/main >../actual) &&
	(cd repo && "$REAL_GIT" rev-parse main >../expect) &&
	test_cmp expect actual
'

test_expect_success 'for-each-ref %(objectname) matches git for all heads' '
	(cd repo && grit for-each-ref --format="%(objectname)" refs/heads/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref --format="%(objectname)" refs/heads/ >../expect) &&
	test_cmp expect actual
'

test_expect_success 'objectname is 40 hex chars' '
	(cd repo && grit for-each-ref --format="%(objectname)" refs/heads/main >../actual) &&
	grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'for-each-ref %(objectname) for tags' '
	(cd repo && grit for-each-ref --format="%(objectname)" refs/tags/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref --format="%(objectname)" refs/tags/ >../expect) &&
	test_cmp expect actual
'

test_expect_success 'annotated tag objectname differs from tagged commit' '
	(cd repo &&
	 TAG_OBJ=$(grit for-each-ref --format="%(objectname)" refs/tags/v1.0) &&
	 COMMIT=$("$REAL_GIT" rev-parse main) &&
	 test "$TAG_OBJ" != "$COMMIT")
'

test_expect_success 'lightweight tag objectname equals commit' '
	(cd repo &&
	 TAG_OBJ=$(grit for-each-ref --format="%(objectname)" refs/tags/lightweight-tag) &&
	 COMMIT=$("$REAL_GIT" rev-parse main) &&
	 test "$TAG_OBJ" = "$COMMIT")
'

###########################################################################
# %(objecttype) tests
###########################################################################

test_expect_success 'for-each-ref %(objecttype) is commit for branches' '
	(cd repo && grit for-each-ref --format="%(objecttype)" refs/heads/main >../actual) &&
	echo "commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'for-each-ref %(objecttype) is tag for annotated tags' '
	(cd repo && grit for-each-ref --format="%(objecttype)" refs/tags/v1.0 >../actual) &&
	echo "tag" >expect &&
	test_cmp expect actual
'

test_expect_success 'for-each-ref %(objecttype) is commit for lightweight tags' '
	(cd repo && grit for-each-ref --format="%(objecttype)" refs/tags/lightweight-tag >../actual) &&
	echo "commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'objecttype matches git for all refs' '
	(cd repo && grit for-each-ref --format="%(objecttype)" refs/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref --format="%(objecttype)" refs/ >../expect) &&
	test_cmp expect actual
'

###########################################################################
# %(refname) and %(refname:short) tests
###########################################################################

test_expect_success 'for-each-ref %(refname) shows full ref path' '
	(cd repo && grit for-each-ref --format="%(refname)" refs/heads/main >../actual) &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'for-each-ref %(refname:short) strips prefix' '
	(cd repo && grit for-each-ref --format="%(refname:short)" refs/heads/main >../actual) &&
	echo "main" >expect &&
	test_cmp expect actual
'

test_expect_success 'refname:short for tags strips refs/tags/' '
	(cd repo && grit for-each-ref --format="%(refname:short)" refs/tags/v1.0 >../actual) &&
	echo "v1.0" >expect &&
	test_cmp expect actual
'

test_expect_success 'refname matches git for all refs' '
	(cd repo && grit for-each-ref --format="%(refname)" refs/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref --format="%(refname)" refs/ >../expect) &&
	test_cmp expect actual
'

test_expect_success 'refname:short matches git for all refs' '
	(cd repo && grit for-each-ref --format="%(refname:short)" refs/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref --format="%(refname:short)" refs/ >../expect) &&
	test_cmp expect actual
'

###########################################################################
# %(subject) tests
###########################################################################

test_expect_success 'for-each-ref %(subject) shows commit message' '
	(cd repo && grit for-each-ref --format="%(subject)" refs/heads/main >../actual) &&
	echo "second commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'for-each-ref %(subject) for annotated tag shows tag message' '
	(cd repo && grit for-each-ref --format="%(subject)" refs/tags/v1.0 >../actual) &&
	echo "version 1.0" >expect &&
	test_cmp expect actual
'

test_expect_success 'subject matches git for heads' '
	(cd repo && grit for-each-ref --format="%(subject)" refs/heads/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref --format="%(subject)" refs/heads/ >../expect) &&
	test_cmp expect actual
'

###########################################################################
# Combined format strings
###########################################################################

test_expect_success 'combined format: objectname + refname' '
	(cd repo && grit for-each-ref --format="%(objectname) %(refname)" refs/heads/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref --format="%(objectname) %(refname)" refs/heads/ >../expect) &&
	test_cmp expect actual
'

test_expect_success 'combined format with literal text' '
	(cd repo && grit for-each-ref --format="ref=%(refname:short) type=%(objecttype)" refs/heads/main >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref --format="ref=%(refname:short) type=%(objecttype)" refs/heads/main >../expect) &&
	test_cmp expect actual
'

test_expect_success 'format: objectname objecttype refname subject combined' '
	(cd repo && grit for-each-ref --format="%(objectname) %(objecttype) %(refname) %(subject)" refs/heads/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref --format="%(objectname) %(objecttype) %(refname) %(subject)" refs/heads/ >../expect) &&
	test_cmp expect actual
'

###########################################################################
# Sorting tests
###########################################################################

test_expect_success 'for-each-ref --sort=refname' '
	(cd repo && grit for-each-ref --sort=refname --format="%(refname:short)" refs/heads/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref --sort=refname --format="%(refname:short)" refs/heads/ >../expect) &&
	test_cmp expect actual
'

test_expect_success 'for-each-ref --sort=-refname reverses order' '
	(cd repo && grit for-each-ref --sort=-refname --format="%(refname:short)" refs/heads/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref --sort=-refname --format="%(refname:short)" refs/heads/ >../expect) &&
	test_cmp expect actual
'

test_expect_success 'sort by objectname' '
	(cd repo && grit for-each-ref --sort=objectname --format="%(objectname)" refs/heads/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref --sort=objectname --format="%(objectname)" refs/heads/ >../expect) &&
	test_cmp expect actual
'

###########################################################################
# --count tests
###########################################################################

test_expect_success 'for-each-ref --count=1 limits output' '
	(cd repo && grit for-each-ref --count=1 --format="%(refname)" refs/heads/ >../actual) &&
	test_line_count = 1 actual
'

test_expect_success 'for-each-ref --count=2 limits to two' '
	(cd repo && grit for-each-ref --count=2 --format="%(refname)" refs/heads/ >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'count larger than refs returns all' '
	(cd repo && grit for-each-ref --count=100 --format="%(refname)" refs/heads/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref --format="%(refname)" refs/heads/ >../expect) &&
	test_cmp expect actual
'

###########################################################################
# Pattern matching
###########################################################################

test_expect_success 'for-each-ref with refs/heads pattern' '
	(cd repo && grit for-each-ref --format="%(refname)" refs/heads/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref --format="%(refname)" refs/heads/ >../expect) &&
	test_cmp expect actual
'

test_expect_success 'for-each-ref with refs/tags pattern' '
	(cd repo && grit for-each-ref --format="%(refname)" refs/tags/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref --format="%(refname)" refs/tags/ >../expect) &&
	test_cmp expect actual
'

test_expect_success 'for-each-ref with nonexistent pattern produces no output' '
	(cd repo && grit for-each-ref --format="%(refname)" refs/nonexistent/ >../actual) &&
	test_must_be_empty actual
'

###########################################################################
# Default format
###########################################################################

test_expect_success 'default format matches git' '
	(cd repo && grit for-each-ref refs/heads/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref refs/heads/ >../expect) &&
	test_cmp expect actual
'

test_expect_success 'default format for tags matches git' '
	(cd repo && grit for-each-ref refs/tags/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref refs/tags/ >../expect) &&
	test_cmp expect actual
'

test_expect_success 'default format for all refs matches git' '
	(cd repo && grit for-each-ref refs/ >../actual) &&
	(cd repo && "$REAL_GIT" for-each-ref refs/ >../expect) &&
	test_cmp expect actual
'

test_done
