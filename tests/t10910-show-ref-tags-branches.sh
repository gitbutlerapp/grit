#!/bin/sh
# Tests for show-ref with --tags, --branches, --head, --verify, --exists,
# --dereference, --hash, --abbrev, and --quiet options.

test_description='show-ref --tags --branches and related options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with branches and tags' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&

	EMPTY_TREE=$(printf "" | grit hash-object -w -t tree --stdin) &&

	C1=$(grit commit-tree "$EMPTY_TREE" -m "first") &&
	C2=$(grit commit-tree "$EMPTY_TREE" -p "$C1" -m "second") &&
	C3=$(grit commit-tree "$EMPTY_TREE" -p "$C2" -m "third") &&

	grit update-ref refs/heads/main "$C3" &&
	grit update-ref refs/heads/develop "$C2" &&
	grit update-ref refs/heads/feature "$C1" &&

	grit tag v1.0 "$C1" &&
	grit tag v2.0 "$C2" &&
	grit tag -a -m "release v3" v3.0 "$C3" &&
	grit tag -a -m "beta" beta "$C2" &&

	grit symbolic-ref HEAD refs/heads/main &&

	echo "$C1" >"$TRASH_DIRECTORY/oid_C1" &&
	echo "$C2" >"$TRASH_DIRECTORY/oid_C2" &&
	echo "$C3" >"$TRASH_DIRECTORY/oid_C3"
	)
'

# ── Basic show-ref ───────────────────────────────────────────────────────────

test_expect_success 'show-ref lists all refs' '
	(
	cd repo &&
	grit show-ref >actual &&
	grep "refs/heads/" actual &&
	grep "refs/tags/" actual
	)
'

test_expect_success 'show-ref output format is OID SP refname' '
	(
	cd repo &&
	grit show-ref >actual &&
	head -1 actual >first_line &&
	# Should be 40-char hex, space, then ref
	grep -E "^[0-9a-f]{40} refs/" first_line
	)
'

# ── --tags ───────────────────────────────────────────────────────────────────

test_expect_success '--tags shows only tag refs' '
	(
	cd repo &&
	grit show-ref --tags >actual &&
	grep "refs/tags/" actual &&
	! grep "refs/heads/" actual
	)
'

test_expect_success '--tags includes lightweight tags' '
	(
	cd repo &&
	grit show-ref --tags >actual &&
	grep "refs/tags/v1.0" actual
	)
'

test_expect_success '--tags includes annotated tags' '
	(
	cd repo &&
	grit show-ref --tags >actual &&
	grep "refs/tags/v3.0" actual
	)
'

test_expect_success '--tags lists all 4 tags' '
	(
	cd repo &&
	grit show-ref --tags >actual &&
	test_line_count = 4 actual
	)
'

# ── --branches ───────────────────────────────────────────────────────────────

test_expect_success '--branches shows only branch refs' '
	(
	cd repo &&
	grit show-ref --branches >actual &&
	grep "refs/heads/" actual &&
	! grep "refs/tags/" actual
	)
'

test_expect_success '--branches lists all 3 branches' '
	(
	cd repo &&
	grit show-ref --branches >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success '--branches includes main' '
	(
	cd repo &&
	grit show-ref --branches >actual &&
	grep "refs/heads/main" actual
	)
'

test_expect_success '--branches includes develop' '
	(
	cd repo &&
	grit show-ref --branches >actual &&
	grep "refs/heads/develop" actual
	)
'

# ── --head ───────────────────────────────────────────────────────────────────

test_expect_success '--head includes HEAD in output' '
	(
	cd repo &&
	grit show-ref --head >actual &&
	grep "^[0-9a-f]* HEAD$" actual
	)
'

test_expect_success '--head HEAD points to same OID as main' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_C3") &&
	grit show-ref --head >actual &&
	grep "^$C3 HEAD$" actual
	)
'

test_expect_success '--head still lists other refs too' '
	(
	cd repo &&
	grit show-ref --head >actual &&
	grep "refs/heads/" actual
	)
'

# ── --verify ─────────────────────────────────────────────────────────────────

test_expect_success '--verify with full refname succeeds' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/main >actual &&
	grep "refs/heads/main" actual
	)
'

test_expect_success '--verify with nonexistent ref fails' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify refs/heads/nonexistent 2>err
	)
'

test_expect_success '--verify with tag ref succeeds' '
	(
	cd repo &&
	grit show-ref --verify refs/tags/v1.0 >actual &&
	grep "refs/tags/v1.0" actual
	)
'

test_expect_success '--verify shows correct OID' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_C3") &&
	grit show-ref --verify refs/heads/main >actual &&
	grep "^$C3" actual
	)
'

# ── --exists ─────────────────────────────────────────────────────────────────

test_expect_success '--exists returns 0 for existing ref' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/main
	)
'

test_expect_success '--exists returns non-zero for missing ref' '
	(
	cd repo &&
	test_must_fail grit show-ref --exists refs/heads/nonexistent
	)
'

test_expect_success '--exists produces no stdout' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/main >actual &&
	test_must_be_empty actual
	)
'

test_expect_success '--exists works for tags' '
	(
	cd repo &&
	grit show-ref --exists refs/tags/v1.0
	)
'

# ── --dereference / -d ───────────────────────────────────────────────────────

test_expect_success '-d shows peeled annotated tag' '
	(
	cd repo &&
	grit show-ref -d refs/tags/v3.0 >actual &&
	grep "refs/tags/v3.0$" actual &&
	grep "refs/tags/v3.0\^{}$" actual
	)
'

test_expect_success '-d peeled tag points to commit OID' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_C3") &&
	grit show-ref -d refs/tags/v3.0 >actual &&
	grep "^$C3 refs/tags/v3.0\^{}$" actual
	)
'

test_expect_success '-d on lightweight tag shows only one line' '
	(
	cd repo &&
	grit show-ref -d refs/tags/v1.0 >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success '--dereference shows all peeled annotated tags' '
	(
	cd repo &&
	grit show-ref --dereference --tags >actual &&
	grep "\^{}$" actual >peeled &&
	test_line_count = 2 peeled
	)
'

# ── --hash / -s ──────────────────────────────────────────────────────────────

test_expect_success '--hash shows only OIDs' '
	(
	cd repo &&
	grit show-ref --hash refs/heads/main >actual &&
	C3=$(cat "$TRASH_DIRECTORY/oid_C3") &&
	echo "$C3" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--hash with no refname arg shows OIDs for all' '
	(
	cd repo &&
	grit show-ref --hash --branches >actual &&
	test_line_count = 3 actual &&
	grep -E "^[0-9a-f]{40}$" actual
	)
'

# ── --quiet / -q ─────────────────────────────────────────────────────────────

test_expect_success '--quiet produces no output on success' '
	(
	cd repo &&
	grit show-ref --quiet refs/heads/main >actual &&
	test_must_be_empty actual
	)
'

test_expect_success '--quiet returns 0 for existing ref' '
	(
	cd repo &&
	grit show-ref --quiet refs/heads/main
	)
'

test_expect_success '--quiet returns non-zero for missing ref' '
	(
	cd repo &&
	test_must_fail grit show-ref --quiet refs/heads/nonexistent
	)
'

# ── Pattern matching ─────────────────────────────────────────────────────────

test_expect_success 'show-ref with pattern limits output' '
	(
	cd repo &&
	grit show-ref refs/heads/main >actual &&
	test_line_count = 1 actual &&
	grep "refs/heads/main" actual
	)
'

test_expect_success 'show-ref with nonexistent pattern is empty' '
	(
	cd repo &&
	grit show-ref refs/nonexistent/ >actual 2>&1 || true &&
	test_must_be_empty actual
	)
'

# ── Edge cases ───────────────────────────────────────────────────────────────

test_expect_success 'show-ref in empty repo returns non-zero' '
	(
	grit init empty_repo &&
	cd empty_repo &&
	test_must_fail grit show-ref
	)
'

test_expect_success '--verify multiple refs at once' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/main refs/heads/develop >actual &&
	test_line_count = 2 actual
	)
'

test_done
