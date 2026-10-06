#!/bin/sh
#
# Tests for 'grit for-each-ref' — format atoms, sorting, counting,
# and pattern filtering.

test_description='grit for-each-ref format atoms'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup: repo with branches and tags' '
	(
	git init --initial-branch=master repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo first >file &&
	git add file &&
	git commit -m "first commit" &&
	git tag v1.0 &&
	echo second >file &&
	git add file &&
	git commit -m "second commit" &&
	git tag v2.0 &&
	git branch feature &&
	echo third >file &&
	git add file &&
	git commit -m "third commit" &&
	git tag v3.0
	)
'

# ---------------------------------------------------------------------------
# %(refname)
# ---------------------------------------------------------------------------
test_expect_success 'for-each-ref %(refname) lists full ref names' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" >actual &&
	grep "^refs/heads/master" actual &&
	grep "^refs/heads/feature" actual &&
	grep "^refs/tags/v1.0" actual
	)
'

test_expect_success 'for-each-ref %(refname) output is sorted by default' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" >actual &&
	sort actual >sorted &&
	test_cmp sorted actual
	)
'

# ---------------------------------------------------------------------------
# %(refname:short)
# ---------------------------------------------------------------------------
test_expect_success 'for-each-ref %(refname:short) strips refs prefix' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" >actual &&
	grep "^master$" actual &&
	grep "^feature$" actual &&
	grep "^v1.0$" actual
	)
'

test_expect_success 'refname:short for tags omits refs/tags/' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" refs/tags/ >actual &&
	grep "^v1.0$" actual &&
	grep "^v2.0$" actual &&
	grep "^v3.0$" actual &&
	! grep "refs/" actual
	)
'

# ---------------------------------------------------------------------------
# %(objecttype)
# ---------------------------------------------------------------------------
test_expect_success 'for-each-ref %(objecttype) shows commit for branches' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype)" refs/heads/ >actual &&
	grep "^commit$" actual &&
	! grep -v "^commit$" actual
	)
'

test_expect_success 'for-each-ref %(objecttype) shows commit for lightweight tags' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype)" refs/tags/ >actual &&
	grep "^commit$" actual
	)
'

# ---------------------------------------------------------------------------
# %(objectname)
# ---------------------------------------------------------------------------
test_expect_success 'for-each-ref %(objectname) shows 40-char hex' '
	(
	cd repo &&
	grit for-each-ref --format="%(objectname)" >actual &&
	while read oid; do
		len=$(printf "%s" "$oid" | wc -c) &&
		test "$len" -eq 40 || return 1
	done <actual
	)
'

test_expect_success 'objectname matches rev-parse for master' '
	(
	cd repo &&
	expected=$(git rev-parse master) &&
	grit for-each-ref --format="%(objectname)" refs/heads/master >actual &&
	echo "$expected" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'objectname matches rev-parse for tag' '
	(
	cd repo &&
	expected=$(git rev-parse v1.0) &&
	grit for-each-ref --format="%(objectname)" refs/tags/v1.0 >actual &&
	echo "$expected" >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# %(subject)
# ---------------------------------------------------------------------------
test_expect_success 'for-each-ref %(subject) shows commit message first line' '
	(
	cd repo &&
	grit for-each-ref --format="%(subject)" refs/heads/master >actual &&
	echo "third commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'subject for tag points to tagged commit message' '
	(
	cd repo &&
	grit for-each-ref --format="%(subject)" refs/tags/v1.0 >actual &&
	echo "first commit" >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# Multiple atoms in format string
# ---------------------------------------------------------------------------
test_expect_success 'format string with multiple atoms' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname) %(objecttype)" refs/heads/master >actual &&
	echo "refs/heads/master commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format string with literal text between atoms' '
	(
	cd repo &&
	grit for-each-ref --format="ref=%(refname:short) type=%(objecttype)" refs/heads/master >actual &&
	echo "ref=master type=commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format string with only literal text (no atoms)' '
	(
	cd repo &&
	grit for-each-ref --format="hello" refs/heads/master >actual &&
	echo "hello" >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# --sort
# ---------------------------------------------------------------------------
test_expect_success '--sort=refname sorts lexically' '
	(
	cd repo &&
	grit for-each-ref --sort=refname --format="%(refname)" >actual &&
	sort actual >sorted &&
	test_cmp sorted actual
	)
'

test_expect_success '--sort=-refname sorts reverse lexically' '
	(
	cd repo &&
	grit for-each-ref --sort=-refname --format="%(refname)" >actual &&
	sort -r actual >sorted &&
	test_cmp sorted actual
	)
'

# ---------------------------------------------------------------------------
# --count
# ---------------------------------------------------------------------------
test_expect_success '--count=1 returns only one ref' '
	(
	cd repo &&
	grit for-each-ref --count=1 --format="%(refname)" >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success '--count=2 returns two refs' '
	(
	cd repo &&
	grit for-each-ref --count=2 --format="%(refname)" >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success '--count larger than ref count returns all' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" >all &&
	total=$(wc -l <all | tr -d " ") &&
	grit for-each-ref --count=100 --format="%(refname)" >actual &&
	test_line_count = "$total" actual
	)
'

# ---------------------------------------------------------------------------
# Pattern filtering
# ---------------------------------------------------------------------------
test_expect_success 'pattern refs/tags/ filters to tags only' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/tags/ >actual &&
	! grep "refs/heads/" actual &&
	grep "refs/tags/" actual
	)
'

test_expect_success 'pattern refs/heads/ filters to branches only' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/ >actual &&
	! grep "refs/tags/" actual &&
	grep "refs/heads/" actual
	)
'

test_expect_success 'pattern with specific ref matches exactly' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/tags/v2.0 >actual &&
	echo "refs/tags/v2.0" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'non-matching pattern produces empty output' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/nonexistent/ >actual &&
	test_must_be_empty actual
	)
'

# ---------------------------------------------------------------------------
# Combined options
# ---------------------------------------------------------------------------
test_expect_success '--sort and --count combined' '
	(
	cd repo &&
	grit for-each-ref --sort=-refname --count=1 --format="%(refname)" >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success '--count with pattern' '
	(
	cd repo &&
	grit for-each-ref --count=1 --format="%(refname)" refs/tags/ >actual &&
	test_line_count = 1 actual &&
	grep "refs/tags/" actual
	)
'

# ---------------------------------------------------------------------------
# Empty repo edge case
# ---------------------------------------------------------------------------
test_expect_success 'for-each-ref on repo with no refs (after deleting)' '
	(
	git init empty-repo &&
	cd empty-repo &&
	grit for-each-ref --format="%(refname)" >actual &&
	test_must_be_empty actual
	)
'

test_done
