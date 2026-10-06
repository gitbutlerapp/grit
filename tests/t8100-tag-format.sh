#!/bin/sh
# Tests for tag --sort, -l patterns, -n, --contains, annotated vs lightweight.

test_description='tag format, sort, and filter options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with tags' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "first" >file.txt &&
	git add file.txt &&
	git commit -m "first commit" &&
	git tag v1.0 -m "release 1.0" &&
	git tag lightweight-1 &&
	echo "second" >file.txt &&
	git add file.txt &&
	git commit -m "second commit" &&
	git tag v2.0 -m "release 2.0" &&
	git tag v2.0-rc1 &&
	echo "third" >file.txt &&
	git add file.txt &&
	git commit -m "third commit" &&
	git tag v3.0 -m "release 3.0" &&
	git tag alpha-tag &&
	git tag zebra-tag
	)
'

# ── List tags ────────────────────────────────────────────────────────────

test_expect_success 'tag -l: lists all tags' '
	(
	cd repo &&
	git tag -l >out &&
	test $(wc -l <out) -eq 7
	)
'

test_expect_success 'tag with no args lists all tags' '
	(
	cd repo &&
	git tag >out &&
	test $(wc -l <out) -eq 7
	)
'

test_expect_success 'tag -l: output is sorted alphabetically by default' '
	(
	cd repo &&
	git tag -l >out &&
	sort out >expected &&
	test_cmp expected out
	)
'

# ── Pattern matching ─────────────────────────────────────────────────────

test_expect_success 'tag -l pattern: v* matches version tags' '
	(
	cd repo &&
	git tag -l "v*" >out &&
	grep "v1.0" out &&
	grep "v2.0" out &&
	grep "v3.0" out &&
	! grep "alpha-tag" out &&
	! grep "zebra-tag" out
	)
'

test_expect_success 'tag -l pattern: *-rc* matches rc tags' '
	(
	cd repo &&
	git tag -l "*-rc*" >out &&
	grep "v2.0-rc1" out &&
	test_line_count = 1 out
	)
'

test_expect_success 'tag -l pattern: no match yields empty' '
	(
	cd repo &&
	git tag -l "nonexistent*" >out &&
	test_must_be_empty out
	)
'

test_expect_success 'tag -l pattern: exact match' '
	(
	cd repo &&
	git tag -l "v1.0" >out &&
	echo "v1.0" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'tag -l pattern: alpha* matches alpha-tag' '
	(
	cd repo &&
	git tag -l "alpha*" >out &&
	echo "alpha-tag" >expected &&
	test_cmp expected out
	)
'

# ── -n (annotation lines) ───────────────────────────────────────────────

test_expect_success 'tag -n: shows annotation for annotated tags' '
	(
	cd repo &&
	git tag -n >out &&
	grep "v1.0" out | grep "release 1.0"
	)
'

test_expect_success 'tag -n: lightweight tags show commit subject' '
	(
	cd repo &&
	git tag -n >out &&
	grep "lightweight-1" out
	)
'

test_expect_success 'tag -n1: shows 1 line of annotation' '
	(
	cd repo &&
	git tag -n1 >out &&
	grep "v2.0" out | grep "release 2.0"
	)
'

# ── --sort ───────────────────────────────────────────────────────────────

test_expect_success 'tag --sort=refname: ascending alphabetical' '
	(
	cd repo &&
	git tag -l --sort=refname >out &&
	sort out >expected &&
	test_cmp expected out
	)
'

test_expect_success 'tag --sort=-refname: descending alphabetical' '
	(
	cd repo &&
	git tag -l --sort=-refname >out &&
	sort -r out >expected &&
	test_cmp expected out
	)
'

test_expect_success 'tag --sort=version:refname: version sort' '
	(
	cd repo &&
	git tag -l --sort=version:refname "v*" >out &&
	head -1 out >first &&
	grep "v1.0" first
	)
'

test_expect_success 'tag --sort=-version:refname: reverse version sort' '
	(
	cd repo &&
	git tag -l --sort=-version:refname "v*" >out &&
	head -1 out >first &&
	grep "v3.0" first
	)
'

# ── --contains ───────────────────────────────────────────────────────────

test_expect_success 'tag --contains HEAD: tags pointing at or after HEAD' '
	(
	cd repo &&
	git tag --contains HEAD >out &&
	grep "v3.0" out &&
	grep "alpha-tag" out &&
	grep "zebra-tag" out
	)
'

test_expect_success 'tag --contains HEAD: shows tags at HEAD' '
	(
	cd repo &&
	git tag --contains HEAD >out &&
	grep "v3.0" out &&
	grep "alpha-tag" out &&
	grep "zebra-tag" out
	)
'

test_expect_success 'tag --contains HEAD~1: includes v2 tags' '
	(
	cd repo &&
	git tag --contains HEAD~1 >out &&
	grep "v2.0" out &&
	grep "v3.0" out
	)
'

# ── Annotated vs lightweight ─────────────────────────────────────────────

test_expect_success 'annotated tag: has tag object' '
	(
	cd repo &&
	git cat-file -t v1.0 >out &&
	echo "tag" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'lightweight tag: points directly to commit' '
	(
	cd repo &&
	git cat-file -t lightweight-1 >out &&
	echo "commit" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'annotated tag: cat-file shows tag message' '
	(
	cd repo &&
	git cat-file -p v1.0 >out &&
	grep "release 1.0" out
	)
'

# ── Tag creation and deletion ────────────────────────────────────────────

test_expect_success 'tag -d: delete a tag' '
	(
	cd repo &&
	git tag temp-tag &&
	git tag -l "temp-tag" >out &&
	grep "temp-tag" out &&
	git tag -d temp-tag &&
	git tag -l "temp-tag" >out2 &&
	test_must_be_empty out2
	)
'

test_expect_success 'tag -f: force overwrite existing tag' '
	(
	cd repo &&
	git tag force-tag HEAD~1 &&
	old_oid=$(git rev-parse force-tag) &&
	git tag -f force-tag HEAD &&
	new_oid=$(git rev-parse force-tag) &&
	test "$old_oid" != "$new_oid"
	)
'

test_expect_success 'tag -a -m: create annotated with message' '
	(
	cd repo &&
	git tag -a -m "annotated msg" ann-test &&
	git cat-file -t ann-test >out &&
	echo "tag" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'tag -m: implies annotated' '
	(
	cd repo &&
	git tag -m "implied annotated" impl-ann &&
	git cat-file -t impl-ann >out &&
	echo "tag" >expected &&
	test_cmp expected out
	)
'

# ── Edge cases ───────────────────────────────────────────────────────────

test_expect_success 'tag: duplicate name fails without -f' '
	(
	cd repo &&
	test_must_fail git tag v1.0
	)
'

test_expect_success 'tag -l: case sensitive matching' '
	(
	cd repo &&
	git tag -l "V*" >out &&
	test_must_be_empty out
	)
'

test_expect_success 'tag -l: wildcard ? matches single char' '
	(
	cd repo &&
	git tag -l "v?.0" >out &&
	grep "v1.0" out &&
	grep "v2.0" out &&
	grep "v3.0" out
	)
'

test_done
