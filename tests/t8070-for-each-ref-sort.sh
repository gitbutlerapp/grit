#!/bin/sh
# Tests for for-each-ref: sort options, custom formats, count, patterns.

test_description='for-each-ref sort and format options'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with multiple refs' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "alpha content" >alpha.txt &&
	git add alpha.txt &&
	git commit -m "first commit" &&
	git tag v1.0 -m "version 1.0" &&
	git branch branch-a &&
	echo "beta content" >beta.txt &&
	git add beta.txt &&
	git commit -m "second commit" &&
	git tag v2.0 -m "version 2.0" &&
	git branch branch-b &&
	echo "gamma content" >gamma.txt &&
	git add gamma.txt &&
	git commit -m "third commit" &&
	git tag v3.0 &&
	git branch branch-c
	)
'

# ── Default output ───────────────────────────────────────────────────────

test_expect_success 'for-each-ref lists all refs' '
	(
	cd repo &&
	git for-each-ref >out &&
	test $(wc -l <out) -ge 6
	)
'

test_expect_success 'for-each-ref default format has OID TYPE REFNAME' '
	(
	cd repo &&
	git for-each-ref refs/heads/master >out &&
	awk "{print NF}" out | head -1 >nf &&
	test "$(cat nf)" -ge 2
	)
'

# ── --format atoms ───────────────────────────────────────────────────────

test_expect_success 'format: %(refname) shows full ref path' '
	(
	cd repo &&
	git for-each-ref --format="%(refname)" refs/heads/ >out &&
	grep "refs/heads/master" out
	)
'

test_expect_success 'format: %(objectname) shows full SHA-1' '
	(
	cd repo &&
	git for-each-ref --format="%(objectname)" refs/heads/master >out &&
	grep -E "^[0-9a-f]{40}$" out
	)
'

test_expect_success 'format: %(objecttype) shows commit for branches' '
	(
	cd repo &&
	git for-each-ref --format="%(objecttype)" refs/heads/master >out &&
	grep "commit" out
	)
'

test_expect_success 'format: %(objecttype) shows tag for annotated tags' '
	(
	cd repo &&
	git for-each-ref --format="%(objecttype)" refs/tags/v1.0 >out &&
	grep "tag" out
	)
'

test_expect_success 'format: %(objecttype) shows commit for lightweight tags' '
	(
	cd repo &&
	git for-each-ref --format="%(objecttype)" refs/tags/v3.0 >out &&
	grep "commit" out
	)
'

test_expect_success 'format: multiple atoms in one format string' '
	(
	cd repo &&
	git for-each-ref --format="%(objectname) %(objecttype) %(refname)" refs/heads/master >out &&
	oid=$(git rev-parse master) &&
	grep "$oid commit refs/heads/master" out
	)
'

test_expect_success 'format: literal text mixed with atoms' '
	(
	cd repo &&
	git for-each-ref --format="ref=%(refname) type=%(objecttype)" refs/heads/master >out &&
	grep "ref=refs/heads/master type=commit" out
	)
'

# ── --sort by refname ────────────────────────────────────────────────────

test_expect_success 'sort: default sorts by refname ascending' '
	(
	cd repo &&
	git for-each-ref --format="%(refname)" refs/heads/ >out &&
	sort out >expected &&
	test_cmp expected out
	)
'

test_expect_success 'sort: --sort=refname ascending' '
	(
	cd repo &&
	git for-each-ref --sort=refname --format="%(refname)" refs/heads/ >out &&
	sort out >expected &&
	test_cmp expected out
	)
'

test_expect_success 'sort: --sort=-refname descending' '
	(
	cd repo &&
	git for-each-ref --sort=-refname --format="%(refname)" refs/heads/ >out &&
	sort -r out >expected &&
	test_cmp expected out
	)
'

# ── --sort by objectname ─────────────────────────────────────────────────

test_expect_success 'sort: --sort=objectname ascending' '
	(
	cd repo &&
	git for-each-ref --sort=objectname --format="%(objectname)" refs/heads/ >out &&
	sort out >expected &&
	test_cmp expected out
	)
'

test_expect_success 'sort: --sort=-objectname descending' '
	(
	cd repo &&
	git for-each-ref --sort=-objectname --format="%(objectname)" refs/heads/ >out &&
	sort -r out >expected &&
	test_cmp expected out
	)
'

# ── --sort by objecttype ─────────────────────────────────────────────────

test_expect_success 'sort: --sort=objecttype groups refs by type' '
	(
	cd repo &&
	git for-each-ref --sort=objecttype --format="%(objecttype)" >out &&
	sort out >expected &&
	test_cmp expected out
	)
'

test_expect_success 'sort: --sort=-objecttype reverse groups refs by type' '
	(
	cd repo &&
	git for-each-ref --sort=-objecttype --format="%(objecttype)" >out &&
	sort -r out >expected &&
	test_cmp expected out
	)
'

# ── Pattern matching ─────────────────────────────────────────────────────

test_expect_success 'pattern: refs/heads/ filters to branches only' '
	(
	cd repo &&
	git for-each-ref --format="%(refname)" refs/heads/ >out &&
	! grep "refs/tags/" out
	)
'

test_expect_success 'pattern: refs/tags/ filters to tags only' '
	(
	cd repo &&
	git for-each-ref --format="%(refname)" refs/tags/ >out &&
	! grep "refs/heads/" out
	)
'

test_expect_success 'pattern: no match yields empty output' '
	(
	cd repo &&
	git for-each-ref --format="%(refname)" refs/nonexistent/ >out &&
	test_must_be_empty out
	)
'

# ── --count ──────────────────────────────────────────────────────────────

test_expect_success 'count: --count=1 returns exactly one ref' '
	(
	cd repo &&
	git for-each-ref --count=1 >out &&
	test_line_count = 1 out
	)
'

test_expect_success 'count: --count=2 returns exactly two refs' '
	(
	cd repo &&
	git for-each-ref --count=2 >out &&
	test_line_count = 2 out
	)
'

test_expect_success 'count: --count larger than total returns all' '
	(
	cd repo &&
	git for-each-ref >all &&
	total=$(wc -l <all | tr -d " ") &&
	git for-each-ref --count=100 >out &&
	test_line_count = "$total" out
	)
'

# ── Combined sort + format + pattern ─────────────────────────────────────

test_expect_success 'combined: sort + format + pattern for tags' '
	(
	cd repo &&
	git for-each-ref --sort=refname --format="%(refname) %(objecttype)" refs/tags/ >out &&
	head -1 out >first &&
	grep "refs/tags/v1.0" first
	)
'

test_expect_success 'combined: reverse sort + format for branches' '
	(
	cd repo &&
	git for-each-ref --sort=-refname --format="%(refname)" refs/heads/ >out &&
	head -1 out >first &&
	grep "master" first
	)
'

test_expect_success 'combined: sort objectname + count + format' '
	(
	cd repo &&
	git for-each-ref --sort=objectname --count=1 --format="%(objectname)" refs/heads/ >out &&
	test_line_count = 1 out
	)
'

test_expect_success 'format: objectname for lightweight vs annotated tag differs' '
	(
	cd repo &&
	git for-each-ref --format="%(objectname)" refs/tags/v1.0 >ann_oid &&
	git for-each-ref --format="%(objectname)" refs/tags/v3.0 >lw_oid &&
	! test_cmp ann_oid lw_oid
	)
'

test_expect_success 'all refs: heads + tags pattern both work' '
	(
	cd repo &&
	git for-each-ref --format="%(refname)" refs/heads/ refs/tags/ >out &&
	grep "refs/heads/" out &&
	grep "refs/tags/" out
	)
'

test_expect_success 'format: only objectname atom' '
	(
	cd repo &&
	git for-each-ref --format="%(objectname)" refs/heads/master >out &&
	oid=$(git rev-parse master) &&
	echo "$oid" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'format: only refname atom' '
	(
	cd repo &&
	git for-each-ref --format="%(refname)" refs/heads/master >out &&
	echo "refs/heads/master" >expected &&
	test_cmp expected out
	)
'

test_done
