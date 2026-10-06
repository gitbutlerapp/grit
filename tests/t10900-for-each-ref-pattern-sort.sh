#!/bin/sh
# Tests for for-each-ref pattern matching and --sort options.

test_description='for-each-ref pattern matching and --sort'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# Setup: create a repo with branches and tags in various namespaces
test_expect_success 'setup repo with diverse refs' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&

	EMPTY_TREE=$(printf "" | grit hash-object -w -t tree --stdin) &&

	C1=$(grit commit-tree "$EMPTY_TREE" -m "first") &&
	C2=$(grit commit-tree "$EMPTY_TREE" -p "$C1" -m "second") &&
	C3=$(grit commit-tree "$EMPTY_TREE" -p "$C2" -m "third") &&
	C4=$(grit commit-tree "$EMPTY_TREE" -p "$C3" -m "fourth") &&
	C5=$(grit commit-tree "$EMPTY_TREE" -p "$C4" -m "fifth") &&

	grit update-ref refs/heads/main "$C5" &&
	grit update-ref refs/heads/develop "$C4" &&
	grit update-ref refs/heads/feature/alpha "$C3" &&
	grit update-ref refs/heads/feature/beta "$C2" &&
	grit update-ref refs/heads/feature/gamma "$C1" &&
	grit update-ref refs/heads/bugfix/one "$C3" &&
	grit update-ref refs/heads/bugfix/two "$C2" &&
	grit update-ref refs/heads/release/v1.0 "$C4" &&
	grit update-ref refs/heads/release/v2.0 "$C5" &&

	grit tag v1.0 "$C1" &&
	grit tag v1.1 "$C2" &&
	grit tag v2.0 "$C3" &&
	grit tag v2.1 "$C4" &&
	grit tag -a -m "annotated v3.0" v3.0 "$C5" &&
	grit tag -a -m "annotated v3.1" v3.1 "$C5" &&

	grit update-ref refs/notes/commits "$C1" &&

	echo "$C1" >"$TRASH_DIRECTORY/oid_C1" &&
	echo "$C2" >"$TRASH_DIRECTORY/oid_C2" &&
	echo "$C3" >"$TRASH_DIRECTORY/oid_C3" &&
	echo "$C4" >"$TRASH_DIRECTORY/oid_C4" &&
	echo "$C5" >"$TRASH_DIRECTORY/oid_C5"
	)
'

# ── Pattern matching ─────────────────────────────────────────────────────────

test_expect_success 'no pattern lists all refs' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" >actual &&
	test_line_count -gt 10 actual
	)
'

test_expect_success 'pattern refs/heads/ lists only branches' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/ >actual &&
	grep "^refs/heads/" actual &&
	! grep "^refs/tags/" actual
	)
'

test_expect_success 'pattern refs/tags/ lists only tags' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/tags/ >actual &&
	grep "^refs/tags/" actual &&
	! grep "^refs/heads/" actual
	)
'

test_expect_success 'pattern refs/heads/feature/ lists only feature branches' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/feature/ >actual &&
	cat >expect <<-\EOF &&
	refs/heads/feature/alpha
	refs/heads/feature/beta
	refs/heads/feature/gamma
	EOF
	test_cmp expect actual
	)
'

test_expect_success 'pattern refs/heads/bugfix/ lists only bugfix branches' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/bugfix/ >actual &&
	cat >expect <<-\EOF &&
	refs/heads/bugfix/one
	refs/heads/bugfix/two
	EOF
	test_cmp expect actual
	)
'

test_expect_success 'pattern refs/heads/release/ lists only release branches' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/release/ >actual &&
	cat >expect <<-\EOF &&
	refs/heads/release/v1.0
	refs/heads/release/v2.0
	EOF
	test_cmp expect actual
	)
'

test_expect_success 'multiple patterns combine results' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/feature/ refs/heads/bugfix/ >actual &&
	grep "^refs/heads/feature/" actual &&
	grep "^refs/heads/bugfix/" actual &&
	! grep "^refs/heads/main" actual
	)
'

test_expect_success 'pattern with no matches produces empty output' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/nonexistent/ >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'refs/notes/ pattern matches notes refs' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/notes/ >actual &&
	grep "^refs/notes/" actual
	)
'

# ── Sort options ─────────────────────────────────────────────────────────────

test_expect_success '--sort=refname sorts alphabetically' '
	(
	cd repo &&
	grit for-each-ref --sort=refname --format="%(refname)" refs/heads/feature/ >actual &&
	cat >expect <<-\EOF &&
	refs/heads/feature/alpha
	refs/heads/feature/beta
	refs/heads/feature/gamma
	EOF
	test_cmp expect actual
	)
'

test_expect_success '--sort=-refname sorts reverse alphabetically' '
	(
	cd repo &&
	grit for-each-ref --sort=-refname --format="%(refname)" refs/heads/feature/ >actual &&
	cat >expect <<-\EOF &&
	refs/heads/feature/gamma
	refs/heads/feature/beta
	refs/heads/feature/alpha
	EOF
	test_cmp expect actual
	)
'

test_expect_success '--sort=refname on tags sorts correctly' '
	(
	cd repo &&
	grit for-each-ref --sort=refname --format="%(refname)" refs/tags/ >actual &&
	cat >expect <<-\EOF &&
	refs/tags/v1.0
	refs/tags/v1.1
	refs/tags/v2.0
	refs/tags/v2.1
	refs/tags/v3.0
	refs/tags/v3.1
	EOF
	test_cmp expect actual
	)
'

test_expect_success '--sort=-refname on tags sorts reverse' '
	(
	cd repo &&
	grit for-each-ref --sort=-refname --format="%(refname)" refs/tags/ >actual &&
	cat >expect <<-\EOF &&
	refs/tags/v3.1
	refs/tags/v3.0
	refs/tags/v2.1
	refs/tags/v2.0
	refs/tags/v1.1
	refs/tags/v1.0
	EOF
	test_cmp expect actual
	)
'

test_expect_success 'default sort is refname ascending' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/feature/ >actual_default &&
	grit for-each-ref --sort=refname --format="%(refname)" refs/heads/feature/ >actual_explicit &&
	test_cmp actual_default actual_explicit
	)
'

# ── Format tokens ────────────────────────────────────────────────────────────

test_expect_success '%(refname) format works' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/main >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '%(objectname) shows full SHA' '
	(
	cd repo &&
	C5=$(cat "$TRASH_DIRECTORY/oid_C5") &&
	grit for-each-ref --format="%(objectname)" refs/heads/main >actual &&
	echo "$C5" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '%(refname:short) strips prefix' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" refs/heads/main >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '%(refname:short) for tags strips refs/tags/' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" refs/tags/v1.0 >actual &&
	echo "v1.0" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '%(objecttype) for branch shows commit' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype)" refs/heads/main >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '%(objecttype) for annotated tag shows tag' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype)" refs/tags/v3.0 >actual &&
	echo "tag" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '%(objecttype) for lightweight tag shows commit' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype)" refs/tags/v1.0 >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

# ── Combined format and sort ─────────────────────────────────────────────────

test_expect_success 'format with multiple tokens' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname) %(objecttype)" refs/heads/main >actual &&
	echo "refs/heads/main commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format with literal text' '
	(
	cd repo &&
	grit for-each-ref --format="ref=%(refname)" refs/heads/main >actual &&
	echo "ref=refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'sort and format combined on all branches' '
	(
	cd repo &&
	grit for-each-ref --sort=refname --format="%(refname:short)" refs/heads/ >actual &&
	head -1 actual >first &&
	echo "bugfix/one" >expect &&
	test_cmp expect first
	)
'

test_expect_success 'sort reverse and format combined on all branches' '
	(
	cd repo &&
	grit for-each-ref --sort=-refname --format="%(refname:short)" refs/heads/ >actual &&
	head -1 actual >first &&
	echo "release/v2.0" >expect &&
	test_cmp expect first
	)
'

# ── --count ──────────────────────────────────────────────────────────────────

test_expect_success '--count=1 limits output to one ref' '
	(
	cd repo &&
	grit for-each-ref --count=1 --format="%(refname)" refs/heads/ >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success '--count=3 limits output to three refs' '
	(
	cd repo &&
	grit for-each-ref --count=3 --format="%(refname)" refs/heads/ >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success '--count=0 shows nothing' '
	(
	cd repo &&
	grit for-each-ref --count=0 --format="%(refname)" refs/heads/ >actual &&
	test_must_be_empty actual
	)
'

test_expect_success '--count larger than ref count shows all' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/feature/ >all &&
	grit for-each-ref --count=100 --format="%(refname)" refs/heads/feature/ >actual &&
	test_cmp all actual
	)
'

test_expect_success '--sort and --count combined' '
	(
	cd repo &&
	grit for-each-ref --sort=-refname --count=2 --format="%(refname)" refs/tags/ >actual &&
	cat >expect <<-\EOF &&
	refs/tags/v3.1
	refs/tags/v3.0
	EOF
	test_cmp expect actual
	)
'

# ── Edge cases ───────────────────────────────────────────────────────────────

test_expect_success 'for-each-ref in empty repo produces no output' '
	(
	grit init empty &&
	cd empty &&
	grit for-each-ref --format="%(refname)" >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'pattern without trailing slash works as prefix' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/main >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'refs/tags pattern includes both lightweight and annotated' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/tags/ >actual &&
	test_line_count = 6 actual
	)
'

test_done
