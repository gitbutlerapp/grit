#!/bin/sh
# Tests for for-each-ref with complex format strings.

test_description='for-each-ref complex format strings and combinations'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup repository with branches and tags' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&

	echo "alpha" >alpha.txt &&
	git add alpha.txt &&
	git commit -m "first: alpha commit" &&
	git tag v1.0 -m "release one" &&
	git branch feature-a &&

	echo "beta" >beta.txt &&
	git add beta.txt &&
	git commit -m "second: beta commit" &&
	git tag v2.0 -m "release two" &&
	git branch feature-b &&

	echo "gamma" >gamma.txt &&
	git add gamma.txt &&
	git commit -m "third: gamma commit" &&
	git tag lightweight &&
	git branch feature-c &&

	echo "delta" >delta.txt &&
	git add delta.txt &&
	git commit -m "fourth: delta commit" &&
	git tag v3.0 -m "release three" &&
	git branch feature-d
	)
'

###########################################################################
# Section 1: Basic format atoms
###########################################################################

test_expect_success 'format %(objectname) shows full SHA' '
	(
	cd repo &&
	git for-each-ref --format="%(objectname)" refs/heads/master >actual &&
	grep -qE "^[0-9a-f]{40}$" actual
	)
'

test_expect_success 'format %(objecttype) shows commit for branches' '
	(
	cd repo &&
	git for-each-ref --format="%(objecttype)" refs/heads/master >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %(objecttype) shows tag for annotated tags' '
	(
	cd repo &&
	git for-each-ref --format="%(objecttype)" refs/tags/v1.0 >actual &&
	echo "tag" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %(objecttype) shows commit for lightweight tags' '
	(
	cd repo &&
	git for-each-ref --format="%(objecttype)" refs/tags/lightweight >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %(refname) shows full ref path' '
	(
	cd repo &&
	git for-each-ref --format="%(refname)" refs/heads/master >actual &&
	echo "refs/heads/master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %(refname:short) shows short name' '
	(
	cd repo &&
	git for-each-ref --format="%(refname:short)" refs/heads/master >actual &&
	echo "master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %(subject) shows commit message subject' '
	(
	cd repo &&
	git for-each-ref --format="%(subject)" refs/heads/master >actual &&
	echo "fourth: delta commit" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 2: Complex format strings with literal text
###########################################################################

test_expect_success 'format with literal prefix and suffix' '
	(
	cd repo &&
	git for-each-ref --format="[%(refname)]" refs/heads/master >actual &&
	echo "[refs/heads/master]" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format with multiple atoms and separators' '
	(
	cd repo &&
	git for-each-ref --format="%(objecttype)|%(refname)|%(subject)" refs/heads/master >actual &&
	grep "^commit|refs/heads/master|fourth: delta commit$" actual
	)
'

test_expect_success 'format with descriptive labels' '
	(
	cd repo &&
	git for-each-ref --format="ref=%(refname) type=%(objecttype) oid=%(objectname)" refs/heads/master >actual &&
	grep "ref=refs/heads/master" actual &&
	grep "type=commit" actual &&
	grep -E "oid=[0-9a-f]{40}" actual
	)
'

test_expect_success 'format with arrow separator' '
	(
	cd repo &&
	git for-each-ref --format="%(refname) -> %(objectname)" refs/heads/master >actual &&
	grep -E "^refs/heads/master -> [0-9a-f]{40}$" actual
	)
'

test_expect_success 'format with parentheses around atoms' '
	(
	cd repo &&
	git for-each-ref --format="(%(objecttype)) %(refname:short)" refs/heads/master >actual &&
	echo "(commit) master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format with empty literal between atoms' '
	(
	cd repo &&
	git for-each-ref --format="%(objecttype)%(refname)" refs/heads/master >actual &&
	echo "commitrefs/heads/master" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 3: Pattern matching (refs filtering)
###########################################################################

test_expect_success 'for-each-ref refs/heads/ lists all branches' '
	(
	cd repo &&
	git for-each-ref --format="%(refname:short)" refs/heads/ >actual &&
	grep "master" actual &&
	grep "feature-a" actual &&
	grep "feature-b" actual &&
	grep "feature-c" actual &&
	grep "feature-d" actual &&
	test $(wc -l <actual) -eq 5
	)
'

test_expect_success 'for-each-ref refs/tags/ lists all tags' '
	(
	cd repo &&
	git for-each-ref --format="%(refname:short)" refs/tags/ >actual &&
	grep "v1.0" actual &&
	grep "v2.0" actual &&
	grep "v3.0" actual &&
	grep "lightweight" actual &&
	test $(wc -l <actual) -eq 4
	)
'

test_expect_success 'for-each-ref with no pattern lists everything' '
	(
	cd repo &&
	git for-each-ref --format="%(refname)" >actual &&
	grep "refs/heads/" actual &&
	grep "refs/tags/" actual
	)
'

test_expect_success 'for-each-ref with non-matching pattern produces empty output' '
	(
	cd repo &&
	git for-each-ref --format="%(refname)" refs/nonexistent/ >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 4: --sort option
###########################################################################

test_expect_success 'for-each-ref --sort=refname sorts alphabetically' '
	(
	cd repo &&
	git for-each-ref --sort=refname --format="%(refname)" refs/heads/ >actual &&
	sort actual >sorted &&
	test_cmp actual sorted
	)
'

test_expect_success 'for-each-ref --sort=-refname sorts reverse' '
	(
	cd repo &&
	git for-each-ref --sort=-refname --format="%(refname)" refs/heads/ >actual &&
	sort -r actual >sorted &&
	test_cmp actual sorted
	)
'

test_expect_success 'for-each-ref --sort=objecttype groups by type' '
	(
	cd repo &&
	git for-each-ref --sort=objecttype --format="%(objecttype) %(refname)" refs/ >actual &&
	test $(wc -l <actual) -gt 0
	)
'

###########################################################################
# Section 5: --count option
###########################################################################

test_expect_success 'for-each-ref --count=1 shows only one ref' '
	(
	cd repo &&
	git for-each-ref --count=1 --format="%(refname)" refs/heads/ >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'for-each-ref --count=2 shows two refs' '
	(
	cd repo &&
	git for-each-ref --count=2 --format="%(refname)" refs/heads/ >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'for-each-ref --count=0 shows zero refs' '
	(
	cd repo &&
	git for-each-ref --count=0 --format="%(refname)" refs/heads/ >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 6: Combining sort, count, and format
###########################################################################

test_expect_success 'sort + count + format combined' '
	(
	cd repo &&
	git for-each-ref --sort=refname --count=2 --format="%(refname:short)" refs/heads/ >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'reverse sort + format on tags' '
	(
	cd repo &&
	git for-each-ref --sort=-refname --format="%(objecttype) %(refname:short)" refs/tags/ >actual &&
	head -1 actual >first &&
	grep "v3.0" first
	)
'

test_expect_success 'format with subject on tags vs branches' '
	(
	cd repo &&
	git for-each-ref --format="%(refname:short): %(subject)" refs/tags/v1.0 >actual &&
	echo "v1.0: release one" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'annotated tag subject vs lightweight tag subject' '
	(
	cd repo &&
	git for-each-ref --format="%(subject)" refs/tags/v1.0 >annotated &&
	git for-each-ref --format="%(subject)" refs/tags/lightweight >light &&
	! test_cmp annotated light
	)
'

###########################################################################
# Section 7: Multiple ref patterns
###########################################################################

test_expect_success 'for-each-ref with multiple patterns' '
	(
	cd repo &&
	git for-each-ref --format="%(refname)" refs/heads/ refs/tags/ >actual &&
	head_count=$(grep -c "refs/heads/" actual) &&
	tag_count=$(grep -c "refs/tags/" actual) &&
	test "$head_count" -eq 5 &&
	test "$tag_count" -eq 4
	)
'

###########################################################################
# Section 8: Edge cases
###########################################################################

test_expect_success 'for-each-ref on empty repo produces no output' '
	(
	git init empty &&
	cd empty &&
	git for-each-ref --format="%(refname)" >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'for-each-ref format with only literal text' '
	(
	cd repo &&
	git for-each-ref --format="hello" refs/heads/master >actual &&
	echo "hello" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref format objectname matches rev-parse' '
	(
	cd repo &&
	git for-each-ref --format="%(objectname)" refs/heads/master >fer_oid &&
	git rev-parse refs/heads/master >rp_oid &&
	test_cmp fer_oid rp_oid
	)
'

test_done
