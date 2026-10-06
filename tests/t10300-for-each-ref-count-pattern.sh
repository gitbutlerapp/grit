#!/bin/sh
# Tests for for-each-ref --count, pattern matching, --sort, --exclude,
# --points-at, and format atom combinations.

test_description='for-each-ref --count, pattern, sort, exclude, points-at'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with multiple refs' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&

	EMPTY_TREE=$(printf "" | grit hash-object -w -t tree --stdin) &&

	A=$(grit commit-tree "$EMPTY_TREE" -m "commit A") &&
	B=$(grit commit-tree "$EMPTY_TREE" -p "$A" -m "commit B") &&
	C=$(grit commit-tree "$EMPTY_TREE" -p "$B" -m "commit C") &&
	D=$(grit commit-tree "$EMPTY_TREE" -p "$C" -m "commit D") &&
	E=$(grit commit-tree "$EMPTY_TREE" -p "$D" -m "commit E") &&

	grit update-ref refs/heads/main "$E" &&
	grit update-ref refs/heads/alpha "$A" &&
	grit update-ref refs/heads/beta "$B" &&
	grit update-ref refs/heads/gamma "$C" &&
	grit update-ref refs/heads/delta "$D" &&
	grit update-ref refs/tags/v1.0 "$A" &&
	grit update-ref refs/tags/v2.0 "$B" &&
	grit update-ref refs/tags/v3.0 "$C" &&
	grit update-ref refs/remotes/origin/main "$E" &&
	grit update-ref refs/remotes/origin/dev "$B" &&

	echo "$A" >"$TRASH_DIRECTORY/oid_A" &&
	echo "$B" >"$TRASH_DIRECTORY/oid_B" &&
	echo "$C" >"$TRASH_DIRECTORY/oid_C" &&
	echo "$D" >"$TRASH_DIRECTORY/oid_D" &&
	echo "$E" >"$TRASH_DIRECTORY/oid_E"
	)
'

# ── --count limits ────────────────────────────────────────────────────────────

test_expect_success '--count=1 returns exactly one ref' '
	(
	cd repo &&
	grit for-each-ref --count=1 --format="%(refname)" refs/heads >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success '--count=3 returns exactly three refs' '
	(
	cd repo &&
	grit for-each-ref --count=3 --format="%(refname)" refs/heads >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success '--count=0 returns no refs' '
	(
	cd repo &&
	grit for-each-ref --count=0 --format="%(refname)" refs/heads >actual &&
	test_must_be_empty actual
	)
'

test_expect_success '--count larger than total returns all refs' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads >all &&
	total=$(wc -l <all) &&
	grit for-each-ref --count=100 --format="%(refname)" refs/heads >actual &&
	test_line_count = "$total" actual
	)
'

test_expect_success '--count=1 with --sort=refname picks first sorted' '
	(
	cd repo &&
	grit for-each-ref --count=1 --sort=refname --format="%(refname)" refs/heads >actual &&
	echo "refs/heads/alpha" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--count=1 with --sort=-refname picks last sorted (reverse)' '
	(
	cd repo &&
	grit for-each-ref --count=1 --sort=-refname --format="%(refname)" refs/heads >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--count=2 with --sort=-refname returns top 2' '
	(
	cd repo &&
	grit for-each-ref --count=2 --sort=-refname --format="%(refname)" refs/heads >actual &&
	test_line_count = 2 actual &&
	head -1 actual >first &&
	grep refs/heads/main first
	)
'

# ── pattern matching ──────────────────────────────────────────────────────────

test_expect_success 'pattern refs/heads filters to heads only' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads >actual &&
	! grep refs/tags actual &&
	! grep refs/remotes actual
	)
'

test_expect_success 'pattern refs/tags filters to tags only' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/tags >actual &&
	! grep refs/heads actual &&
	! grep refs/remotes actual
	)
'

test_expect_success 'pattern refs/remotes filters to remotes only' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/remotes >actual &&
	! grep refs/heads actual &&
	! grep refs/tags actual
	)
'

test_expect_success 'no pattern lists all refs' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" >actual &&
	grep refs/heads actual &&
	grep refs/tags actual &&
	grep refs/remotes actual
	)
'

test_expect_success 'multiple patterns combine results' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads refs/tags >actual &&
	grep refs/heads actual &&
	grep refs/tags actual &&
	! grep refs/remotes actual
	)
'

test_expect_success 'pattern with no match produces empty output' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/nonexistent >actual &&
	test_must_be_empty actual
	)
'

# ── --sort ────────────────────────────────────────────────────────────────────

test_expect_success '--sort=refname sorts ascending' '
	(
	cd repo &&
	grit for-each-ref --sort=refname --format="%(refname)" refs/heads >actual &&
	sort actual >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--sort=-refname sorts descending' '
	(
	cd repo &&
	grit for-each-ref --sort=-refname --format="%(refname)" refs/heads >actual &&
	sort -r actual >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--sort=refname on tags' '
	(
	cd repo &&
	grit for-each-ref --sort=refname --format="%(refname)" refs/tags >actual &&
	sort actual >expect &&
	test_cmp expect actual
	)
'

# ── --exclude ─────────────────────────────────────────────────────────────────

test_expect_success '--exclude removes matching ref' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" --exclude=refs/heads/main refs/heads >actual &&
	! grep refs/heads/main actual &&
	grep refs/heads/alpha actual
	)
'

test_expect_success '--exclude with multiple refs' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" --exclude=refs/heads/alpha --exclude=refs/heads/beta refs/heads >actual &&
	! grep refs/heads/alpha actual &&
	! grep refs/heads/beta actual &&
	grep refs/heads/gamma actual
	)
'

test_expect_success '--exclude with no match still shows all' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" --exclude=refs/heads/nonexistent refs/heads >all &&
	grit for-each-ref --format="%(refname)" refs/heads >expected &&
	test_cmp expected all
	)
'

test_expect_success '--exclude combined with --count' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" --exclude=refs/heads/main --count=2 refs/heads >actual &&
	test_line_count = 2 actual &&
	! grep refs/heads/main actual
	)
'

# ── --points-at ───────────────────────────────────────────────────────────────

test_expect_success '--points-at with specific commit' '
	(
	cd repo &&
	A=$(cat "$TRASH_DIRECTORY/oid_A") &&
	grit for-each-ref --points-at="$A" --format="%(refname)" >actual &&
	grep refs/heads/alpha actual &&
	grep refs/tags/v1.0 actual
	)
'

test_expect_success '--points-at filters out non-matching refs' '
	(
	cd repo &&
	A=$(cat "$TRASH_DIRECTORY/oid_A") &&
	grit for-each-ref --points-at="$A" --format="%(refname)" refs/heads >actual &&
	! grep refs/heads/main actual &&
	! grep refs/heads/beta actual
	)
'

test_expect_success '--points-at with latest commit' '
	(
	cd repo &&
	E=$(cat "$TRASH_DIRECTORY/oid_E") &&
	grit for-each-ref --points-at="$E" --format="%(refname)" refs/heads >actual &&
	grep refs/heads/main actual
	)
'

test_expect_success '--points-at combined with pattern' '
	(
	cd repo &&
	B=$(cat "$TRASH_DIRECTORY/oid_B") &&
	grit for-each-ref --points-at="$B" --format="%(refname)" refs/tags >actual &&
	grep refs/tags/v2.0 actual &&
	! grep refs/heads actual
	)
'

# ── format atoms ──────────────────────────────────────────────────────────────

test_expect_success '%(refname) shows full refname' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/main >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '%(refname:short) shows short refname' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" refs/heads/main >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '%(objectname) shows full SHA' '
	(
	cd repo &&
	E=$(cat "$TRASH_DIRECTORY/oid_E") &&
	grit for-each-ref --format="%(objectname)" refs/heads/main >actual &&
	echo "$E" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '%(objecttype) shows commit for branches' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype)" refs/heads/main >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'multiple format atoms in one string' '
	(
	cd repo &&
	E=$(cat "$TRASH_DIRECTORY/oid_E") &&
	grit for-each-ref --format="%(refname) %(objecttype) %(objectname)" refs/heads/main >actual &&
	echo "refs/heads/main commit $E" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format with literal text' '
	(
	cd repo &&
	grit for-each-ref --format="ref=%(refname:short)" refs/heads/main >actual &&
	echo "ref=main" >expect &&
	test_cmp expect actual
	)
'

# ── combined operations ───────────────────────────────────────────────────────

test_expect_success '--count with pattern and --sort' '
	(
	cd repo &&
	grit for-each-ref --count=2 --sort=refname --format="%(refname:short)" refs/heads >actual &&
	test_line_count = 2 actual &&
	head -1 actual >first &&
	echo "alpha" >expect &&
	test_cmp expect first
	)
'

test_expect_success '--count with --exclude and --sort' '
	(
	cd repo &&
	grit for-each-ref --count=1 --sort=refname --exclude=refs/heads/alpha --format="%(refname:short)" refs/heads >actual &&
	echo "beta" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--points-at with --count' '
	(
	cd repo &&
	A=$(cat "$TRASH_DIRECTORY/oid_A") &&
	grit for-each-ref --points-at="$A" --count=1 --format="%(refname)" >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success '--sort with --points-at' '
	(
	cd repo &&
	A=$(cat "$TRASH_DIRECTORY/oid_A") &&
	grit for-each-ref --points-at="$A" --sort=refname --format="%(refname)" >actual &&
	sort actual >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'for-each-ref with empty repo section has no output' '
	(
	grit init empty-repo &&
	cd empty-repo &&
	grit for-each-ref --format="%(refname)" refs/heads >actual &&
	test_must_be_empty actual
	)
'

test_done
