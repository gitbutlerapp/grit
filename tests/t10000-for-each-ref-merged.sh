#!/bin/sh
# Tests for for-each-ref --merged / --no-merged / --contains / --no-contains
# filtering options in combination with patterns, sorting, and counting.

test_description='for-each-ref --merged/--no-merged/--contains/--no-contains'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# Build a diamond DAG:
#
#     A---B---C  (main)
#      \     /
#       D---E    (side)
#      \
#       F        (leaf)
#
# plus tags on each commit.

test_expect_success 'setup diamond DAG' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&

	EMPTY_TREE=$(printf "" | grit hash-object -w -t tree --stdin) &&

	A=$(grit commit-tree "$EMPTY_TREE" -m A) &&
	B=$(grit commit-tree "$EMPTY_TREE" -p "$A" -m B) &&
	D=$(grit commit-tree "$EMPTY_TREE" -p "$A" -m D) &&
	E=$(grit commit-tree "$EMPTY_TREE" -p "$D" -m E) &&
	C=$(grit commit-tree "$EMPTY_TREE" -p "$B" -p "$E" -m C) &&
	F=$(grit commit-tree "$EMPTY_TREE" -p "$A" -m F) &&

	grit update-ref refs/heads/main   "$C" &&
	grit update-ref refs/heads/side   "$E" &&
	grit update-ref refs/heads/leaf   "$F" &&
	grit update-ref refs/heads/root   "$A" &&
	grit update-ref refs/heads/middle "$B" &&

	grit tag -a -m "tag-a" tag-a "$A" &&
	grit tag -a -m "tag-b" tag-b "$B" &&
	grit tag -a -m "tag-c" tag-c "$C" &&
	grit tag lightweight-d "$D" &&
	grit tag lightweight-e "$E" &&
	grit tag lightweight-f "$F" &&

	echo "$A" >"$TRASH_DIRECTORY/oid_A" &&
	echo "$B" >"$TRASH_DIRECTORY/oid_B" &&
	echo "$C" >"$TRASH_DIRECTORY/oid_C" &&
	echo "$D" >"$TRASH_DIRECTORY/oid_D" &&
	echo "$E" >"$TRASH_DIRECTORY/oid_E" &&
	echo "$F" >"$TRASH_DIRECTORY/oid_F"
	)
'

# ── --merged ──────────────────────────────────────────────────────────────────

test_expect_success '--merged=main includes branches reachable from main' '
	(
	cd repo &&
	grit for-each-ref --merged=main --format="%(refname)" refs/heads >actual &&
	cat >expect <<-\EOF &&
	refs/heads/main
	refs/heads/middle
	refs/heads/root
	refs/heads/side
	EOF
	test_cmp expect actual
	)
'

test_expect_success '--merged=main does not include leaf' '
	(
	cd repo &&
	grit for-each-ref --merged=main --format="%(refname)" refs/heads >actual &&
	! grep refs/heads/leaf actual
	)
'

test_expect_success '--merged=side includes only side and root' '
	(
	cd repo &&
	grit for-each-ref --merged=side --format="%(refname)" refs/heads >actual &&
	cat >expect <<-\EOF &&
	refs/heads/root
	refs/heads/side
	EOF
	test_cmp expect actual
	)
'

test_expect_success '--merged=leaf includes only leaf and root' '
	(
	cd repo &&
	grit for-each-ref --merged=leaf --format="%(refname)" refs/heads >actual &&
	cat >expect <<-\EOF &&
	refs/heads/leaf
	refs/heads/root
	EOF
	test_cmp expect actual
	)
'

test_expect_success '--merged=root includes only root itself' '
	(
	cd repo &&
	grit for-each-ref --merged=root --format="%(refname)" refs/heads >actual &&
	cat >expect <<-\EOF &&
	refs/heads/root
	EOF
	test_cmp expect actual
	)
'

test_expect_success '--merged with tags namespace' '
	(
	cd repo &&
	grit for-each-ref --merged=main --format="%(refname)" refs/tags >actual &&
	grep refs/tags/tag-a actual &&
	grep refs/tags/tag-b actual &&
	grep refs/tags/tag-c actual &&
	grep refs/tags/lightweight-d actual &&
	grep refs/tags/lightweight-e actual
	)
'

test_expect_success '--merged with tags does not include lightweight-f' '
	(
	cd repo &&
	grit for-each-ref --merged=main --format="%(refname)" refs/tags >actual &&
	! grep refs/tags/lightweight-f actual
	)
'

# ── --no-merged ───────────────────────────────────────────────────────────────

test_expect_success '--no-merged=main shows only leaf' '
	(
	cd repo &&
	grit for-each-ref --no-merged=main --format="%(refname)" refs/heads >actual &&
	cat >expect <<-\EOF &&
	refs/heads/leaf
	EOF
	test_cmp expect actual
	)
'

test_expect_success '--no-merged=side shows branches not reachable from side' '
	(
	cd repo &&
	grit for-each-ref --no-merged=side --format="%(refname)" refs/heads >actual &&
	grep refs/heads/main actual &&
	grep refs/heads/middle actual &&
	grep refs/heads/leaf actual
	)
'

test_expect_success '--no-merged=root shows everything except root' '
	(
	cd repo &&
	grit for-each-ref --no-merged=root --format="%(refname)" refs/heads >actual &&
	grep refs/heads/main actual &&
	grep refs/heads/side actual &&
	grep refs/heads/leaf actual &&
	grep refs/heads/middle actual &&
	! grep "refs/heads/root$" actual
	)
'

test_expect_success '--no-merged with tags namespace' '
	(
	cd repo &&
	grit for-each-ref --no-merged=main --format="%(refname)" refs/tags >actual &&
	grep refs/tags/lightweight-f actual
	)
'

# ── --contains ────────────────────────────────────────────────────────────────

test_expect_success '--contains with root commit matches all branches' '
	(
	cd repo &&
	A=$(cat "$TRASH_DIRECTORY/oid_A") &&
	grit for-each-ref --contains="$A" --format="%(refname)" refs/heads >actual &&
	test_line_count = 5 actual
	)
'

test_expect_success '--contains with leaf commit matches only leaf' '
	(
	cd repo &&
	F=$(cat "$TRASH_DIRECTORY/oid_F") &&
	grit for-each-ref --contains="$F" --format="%(refname)" refs/heads >actual &&
	cat >expect <<-\EOF &&
	refs/heads/leaf
	EOF
	test_cmp expect actual
	)
'

test_expect_success '--contains with merge commit matches only main' '
	(
	cd repo &&
	C=$(cat "$TRASH_DIRECTORY/oid_C") &&
	grit for-each-ref --contains="$C" --format="%(refname)" refs/heads >actual &&
	cat >expect <<-\EOF &&
	refs/heads/main
	EOF
	test_cmp expect actual
	)
'

test_expect_success '--contains with B matches main and middle' '
	(
	cd repo &&
	B=$(cat "$TRASH_DIRECTORY/oid_B") &&
	grit for-each-ref --contains="$B" --format="%(refname)" refs/heads >actual &&
	grep refs/heads/main actual &&
	grep refs/heads/middle actual &&
	test_line_count = 2 actual
	)
'

test_expect_success '--contains with D matches main and side' '
	(
	cd repo &&
	D=$(cat "$TRASH_DIRECTORY/oid_D") &&
	grit for-each-ref --contains="$D" --format="%(refname)" refs/heads >actual &&
	grep refs/heads/main actual &&
	grep refs/heads/side actual
	)
'

test_expect_success '--contains against tags' '
	(
	cd repo &&
	A=$(cat "$TRASH_DIRECTORY/oid_A") &&
	grit for-each-ref --contains="$A" --format="%(refname)" refs/tags >actual &&
	test_line_count = 6 actual
	)
'

# ── --no-contains ─────────────────────────────────────────────────────────────

test_expect_success '--no-contains root commit matches nothing' '
	(
	cd repo &&
	A=$(cat "$TRASH_DIRECTORY/oid_A") &&
	grit for-each-ref --no-contains="$A" --format="%(refname)" refs/heads >actual &&
	test_must_be_empty actual
	)
'

test_expect_success '--no-contains merge commit matches most branches' '
	(
	cd repo &&
	C=$(cat "$TRASH_DIRECTORY/oid_C") &&
	grit for-each-ref --no-contains="$C" --format="%(refname)" refs/heads >actual &&
	! grep "refs/heads/main$" actual &&
	grep refs/heads/leaf actual &&
	grep refs/heads/side actual
	)
'

test_expect_success '--no-contains F matches everything except leaf' '
	(
	cd repo &&
	F=$(cat "$TRASH_DIRECTORY/oid_F") &&
	grit for-each-ref --no-contains="$F" --format="%(refname)" refs/heads >actual &&
	! grep "refs/heads/leaf$" actual &&
	grep refs/heads/main actual
	)
'

# ── Combined with other options ───────────────────────────────────────────────

test_expect_success '--merged combined with --sort=-refname' '
	(
	cd repo &&
	grit for-each-ref --merged=main --format="%(refname)" --sort=-refname refs/heads >actual &&
	head -1 actual >first &&
	grep refs/heads/side first
	)
'

test_expect_success '--merged combined with --count' '
	(
	cd repo &&
	grit for-each-ref --merged=main --format="%(refname)" --count=2 refs/heads >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success '--no-merged combined with pattern' '
	(
	cd repo &&
	grit for-each-ref --no-merged=side --format="%(refname)" refs/heads >actual &&
	grep refs/heads/leaf actual
	)
'

test_expect_success '--merged with full SHA ref argument' '
	(
	cd repo &&
	C=$(cat "$TRASH_DIRECTORY/oid_C") &&
	grit for-each-ref --merged="$C" --format="%(refname)" refs/heads >actual &&
	grep refs/heads/main actual &&
	grep refs/heads/side actual &&
	grep refs/heads/root actual
	)
'

test_expect_success '--contains with pattern filter' '
	(
	cd repo &&
	B=$(cat "$TRASH_DIRECTORY/oid_B") &&
	grit for-each-ref --contains="$B" --format="%(refname)" refs/heads >actual &&
	grep refs/heads/main actual &&
	grep refs/heads/middle actual
	)
'

test_expect_success '--no-merged excludes results from --count' '
	(
	cd repo &&
	grit for-each-ref --no-merged=main --format="%(refname)" --count=10 refs/heads >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success '--merged with --exclude' '
	(
	cd repo &&
	grit for-each-ref --merged=main --format="%(refname)" --exclude=refs/heads/root refs/heads >actual &&
	! grep refs/heads/root actual &&
	grep refs/heads/main actual
	)
'

test_expect_success '--merged=main with all namespaces' '
	(
	cd repo &&
	grit for-each-ref --merged=main --format="%(refname)" >actual &&
	grep refs/heads/main actual &&
	grep refs/tags/tag-a actual
	)
'

test_expect_success '--no-merged=main with all namespaces' '
	(
	cd repo &&
	grit for-each-ref --no-merged=main --format="%(refname)" >actual &&
	grep refs/heads/leaf actual &&
	grep refs/tags/lightweight-f actual
	)
'

test_expect_success '--contains with %(objecttype) format' '
	(
	cd repo &&
	A=$(cat "$TRASH_DIRECTORY/oid_A") &&
	grit for-each-ref --contains="$A" --format="%(refname) %(objecttype)" refs/heads >actual &&
	grep "refs/heads/main commit" actual
	)
'

test_expect_success '--merged shows annotated tags properly' '
	(
	cd repo &&
	grit for-each-ref --merged=main --format="%(refname) %(objecttype)" refs/tags >actual &&
	grep "refs/tags/tag-a tag" actual &&
	grep "refs/tags/lightweight-d commit" actual
	)
'

test_done
