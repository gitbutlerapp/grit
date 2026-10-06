#!/bin/sh
# Tests for grit for-each-ref --exclude patterns and filtering.

test_description='grit for-each-ref --exclude patterns'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with many refs' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "initial" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial commit" &&
	"$REAL_GIT" branch feature/alpha &&
	"$REAL_GIT" branch feature/beta &&
	"$REAL_GIT" branch feature/gamma &&
	"$REAL_GIT" branch bugfix/issue-1 &&
	"$REAL_GIT" branch bugfix/issue-2 &&
	"$REAL_GIT" branch release/v1.0 &&
	"$REAL_GIT" branch release/v2.0 &&
	"$REAL_GIT" tag v1.0 &&
	"$REAL_GIT" tag v2.0 &&
	"$REAL_GIT" tag v3.0-rc1
	)
'

###########################################################################
# Section 2: Basic for-each-ref output
###########################################################################

test_expect_success 'for-each-ref lists all refs' '
	(
	cd repo &&
	grit for-each-ref >actual &&
	test -s actual
	)
'

test_expect_success 'for-each-ref output matches git' '
	(
	cd repo &&
	grit for-each-ref >actual &&
	"$REAL_GIT" for-each-ref >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref with refs/heads/ pattern' '
	(
	cd repo &&
	grit for-each-ref refs/heads/ >actual &&
	"$REAL_GIT" for-each-ref refs/heads/ >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref with refs/tags/ pattern' '
	(
	cd repo &&
	grit for-each-ref refs/tags/ >actual &&
	"$REAL_GIT" for-each-ref refs/tags/ >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref with specific pattern refs/heads/feature/' '
	(
	cd repo &&
	grit for-each-ref refs/heads/feature/ >actual &&
	"$REAL_GIT" for-each-ref refs/heads/feature/ >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref feature pattern shows exactly 3 refs' '
	(
	cd repo &&
	grit for-each-ref refs/heads/feature/ >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'for-each-ref bugfix pattern shows exactly 2 refs' '
	(
	cd repo &&
	grit for-each-ref refs/heads/bugfix/ >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'for-each-ref release pattern shows exactly 2 refs' '
	(
	cd repo &&
	grit for-each-ref refs/heads/release/ >actual &&
	test_line_count = 2 actual
	)
'

###########################################################################
# Section 3: --exclude filtering
###########################################################################

test_expect_success 'for-each-ref --exclude refs/heads/feature/ excludes feature branches' '
	(
	cd repo &&
	grit for-each-ref --exclude=refs/heads/feature/ refs/heads/ >actual &&
	"$REAL_GIT" for-each-ref --exclude=refs/heads/feature/ refs/heads/ >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'excluded feature branches not present in output' '
	(
	cd repo &&
	grit for-each-ref --exclude=refs/heads/feature/ refs/heads/ >actual &&
	! grep "refs/heads/feature/" actual
	)
'

test_expect_success 'non-excluded branches still present after exclude' '
	(
	cd repo &&
	grit for-each-ref --exclude=refs/heads/feature/ refs/heads/ >actual &&
	grep "refs/heads/bugfix/" actual &&
	grep "refs/heads/release/" actual
	)
'

test_expect_success 'for-each-ref --exclude refs/heads/bugfix/' '
	(
	cd repo &&
	grit for-each-ref --exclude=refs/heads/bugfix/ refs/heads/ >actual &&
	"$REAL_GIT" for-each-ref --exclude=refs/heads/bugfix/ refs/heads/ >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref --exclude all heads still shows tags' '
	(
	cd repo &&
	grit for-each-ref --exclude=refs/heads/ >actual &&
	"$REAL_GIT" for-each-ref --exclude=refs/heads/ >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'exclude all heads removes all branch lines' '
	(
	cd repo &&
	grit for-each-ref --exclude=refs/heads/ >actual &&
	! grep "refs/heads/" actual
	)
'

test_expect_success 'exclude tags still shows heads' '
	(
	cd repo &&
	grit for-each-ref --exclude=refs/tags/ >actual &&
	"$REAL_GIT" for-each-ref --exclude=refs/tags/ >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'exclude tags removes all tag lines' '
	(
	cd repo &&
	grit for-each-ref --exclude=refs/tags/ >actual &&
	! grep "refs/tags/" actual
	)
'

###########################################################################
# Section 4: --format with for-each-ref
###########################################################################

test_expect_success 'for-each-ref --format refname' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" >actual &&
	"$REAL_GIT" for-each-ref --format="%(refname)" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref --format refname:short' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" >actual &&
	"$REAL_GIT" for-each-ref --format="%(refname:short)" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref --format objectname' '
	(
	cd repo &&
	grit for-each-ref --format="%(objectname)" >actual &&
	"$REAL_GIT" for-each-ref --format="%(objectname)" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref --format objecttype' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype)" >actual &&
	"$REAL_GIT" for-each-ref --format="%(objecttype)" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref --format combined fields' '
	(
	cd repo &&
	grit for-each-ref --format="%(objectname) %(refname)" >actual &&
	"$REAL_GIT" for-each-ref --format="%(objectname) %(refname)" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref --format with literal text' '
	(
	cd repo &&
	grit for-each-ref --format="ref=%(refname)" refs/heads/ >actual &&
	"$REAL_GIT" for-each-ref --format="ref=%(refname)" refs/heads/ >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 5: --sort option
###########################################################################

test_expect_success 'for-each-ref --sort=refname' '
	(
	cd repo &&
	grit for-each-ref --sort=refname >actual &&
	"$REAL_GIT" for-each-ref --sort=refname >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref --sort=-refname (reverse)' '
	(
	cd repo &&
	grit for-each-ref --sort=-refname >actual &&
	"$REAL_GIT" for-each-ref --sort=-refname >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref --sort=objectname' '
	(
	cd repo &&
	grit for-each-ref --sort=objectname refs/heads/ >actual &&
	"$REAL_GIT" for-each-ref --sort=objectname refs/heads/ >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 6: --count option
###########################################################################

test_expect_success 'for-each-ref --count=1 shows single ref' '
	(
	cd repo &&
	grit for-each-ref --count=1 >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'for-each-ref --count=3 shows three refs' '
	(
	cd repo &&
	grit for-each-ref --count=3 >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'for-each-ref --count matches git' '
	(
	cd repo &&
	grit for-each-ref --count=2 refs/heads/ >actual &&
	"$REAL_GIT" for-each-ref --count=2 refs/heads/ >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 7: --points-at filtering
###########################################################################

test_expect_success 'for-each-ref --points-at HEAD' '
	(
	cd repo &&
	grit for-each-ref --points-at=HEAD >actual &&
	"$REAL_GIT" for-each-ref --points-at=HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref --points-at specific commit' '
	(
	cd repo &&
	OID=$("$REAL_GIT" rev-parse HEAD) &&
	grit for-each-ref --points-at=$OID >actual &&
	"$REAL_GIT" for-each-ref --points-at=$OID >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 8: Multiple patterns
###########################################################################

test_expect_success 'for-each-ref with multiple patterns' '
	(
	cd repo &&
	grit for-each-ref refs/heads/feature/ refs/tags/ >actual &&
	"$REAL_GIT" for-each-ref refs/heads/feature/ refs/tags/ >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref with disjoint patterns' '
	(
	cd repo &&
	grit for-each-ref refs/heads/bugfix/ refs/heads/release/ >actual &&
	"$REAL_GIT" for-each-ref refs/heads/bugfix/ refs/heads/release/ >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref nonexistent pattern yields empty' '
	(
	cd repo &&
	grit for-each-ref refs/heads/nonexistent/ >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'for-each-ref empty repo has no refs' '
	(
	"$REAL_GIT" init empty-repo &&
	cd empty-repo &&
	grit for-each-ref >actual &&
	test_must_be_empty actual
	)
'

test_done
