#!/bin/sh
# Tests for show-ref --quiet, --hash, --abbrev, --verify, --exists,
# --tags, --branches, --head, and combinations.

test_description='show-ref --quiet, --hash, --abbrev, --verify, --exists'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with refs' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&

	EMPTY_TREE=$(printf "" | grit hash-object -w -t tree --stdin) &&

	A=$(grit commit-tree "$EMPTY_TREE" -m "first") &&
	B=$(grit commit-tree "$EMPTY_TREE" -p "$A" -m "second") &&
	C=$(grit commit-tree "$EMPTY_TREE" -p "$B" -m "third") &&

	grit update-ref refs/heads/main "$C" &&
	grit update-ref refs/heads/develop "$B" &&
	grit update-ref refs/heads/feature "$A" &&

	git symbolic-ref HEAD refs/heads/main &&

	grit tag -a -m "release v1" v1.0 "$A" &&
	grit tag -a -m "release v2" v2.0 "$B" &&
	grit tag lightweight "$C" &&

	echo "$A" >"$TRASH_DIRECTORY/oid_A" &&
	echo "$B" >"$TRASH_DIRECTORY/oid_B" &&
	echo "$C" >"$TRASH_DIRECTORY/oid_C"
	)
'

# ── --quiet ───────────────────────────────────────────────────────────────────

test_expect_success '--quiet with matching ref produces no output' '
	(
	cd repo &&
	grit show-ref --quiet refs/heads/main >actual &&
	test_must_be_empty actual
	)
'

test_expect_success '--quiet succeeds (exit 0) for existing ref' '
	(
	cd repo &&
	grit show-ref --quiet refs/heads/main
	)
'

test_expect_success '--quiet fails (exit non-zero) for nonexistent ref' '
	(
	cd repo &&
	test_must_fail grit show-ref --quiet refs/heads/nonexistent
	)
'

test_expect_success '--quiet with pattern matching' '
	(
	cd repo &&
	grit show-ref --quiet main >actual &&
	test_must_be_empty actual
	)
'

test_expect_success '-q is alias for --quiet' '
	(
	cd repo &&
	grit show-ref -q refs/heads/main >actual &&
	test_must_be_empty actual
	)
'

# ── --hash ────────────────────────────────────────────────────────────────────

test_expect_success '--hash shows only SHA (no refname)' '
	(
	cd repo &&
	C=$(cat "$TRASH_DIRECTORY/oid_C") &&
	grit show-ref --hash refs/heads/main >actual &&
	echo "$C" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--hash with multiple refs shows one SHA per line' '
	(
	cd repo &&
	grit show-ref --branches --hash >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success '--hash output contains no spaces (no refname)' '
	(
	cd repo &&
	grit show-ref --branches --hash >actual &&
	! grep " " actual
	)
'

test_expect_success '-s is alias for --hash' '
	(
	cd repo &&
	grit show-ref --hash refs/heads/main >expected &&
	grit show-ref -s refs/heads/main >actual &&
	test_cmp expected actual
	)
'

test_expect_success '--hash with pattern' '
	(
	cd repo &&
	A=$(cat "$TRASH_DIRECTORY/oid_A") &&
	grit show-ref --hash refs/heads/feature >actual &&
	echo "$A" >expect &&
	test_cmp expect actual
	)
'

# ── --abbrev ──────────────────────────────────────────────────────────────────

test_expect_success '--abbrev shows abbreviated SHA' '
	(
	cd repo &&
	grit show-ref --abbrev refs/heads/main >actual &&
	len=$(awk "{print length(\$1)}" actual) &&
	test "$len" -lt 40
	)
'

test_expect_success '--abbrev=7 shows 7-char prefix' '
	(
	cd repo &&
	C=$(cat "$TRASH_DIRECTORY/oid_C") &&
	grit show-ref --abbrev=7 refs/heads/main >actual &&
	short=$(echo "$C" | cut -c1-7) &&
	grep "^$short" actual
	)
'

test_expect_success '--hash combined with --abbrev' '
	(
	cd repo &&
	grit show-ref --hash --abbrev refs/heads/main >actual &&
	len=$(awk "{print length(\$1)}" actual) &&
	test "$len" -lt 40 &&
	! grep " " actual
	)
'

test_expect_success '--hash --abbrev=10 shows 10-char hash only' '
	(
	cd repo &&
	C=$(cat "$TRASH_DIRECTORY/oid_C") &&
	grit show-ref --hash --abbrev=10 refs/heads/main >actual &&
	short=$(echo "$C" | cut -c1-10) &&
	echo "$short" >expect &&
	test_cmp expect actual
	)
'

# ── --verify ──────────────────────────────────────────────────────────────────

test_expect_success '--verify with full refname succeeds' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/main >actual &&
	grep refs/heads/main actual
	)
'

test_expect_success '--verify with nonexistent ref fails' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify refs/heads/nonexistent
	)
'

test_expect_success '--verify requires full refname (bare name fails)' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify main 2>err
	)
'

test_expect_success '--verify with multiple refs' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/main refs/heads/develop >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success '--verify --quiet produces no output on success' '
	(
	cd repo &&
	grit show-ref --verify --quiet refs/heads/main >actual &&
	test_must_be_empty actual
	)
'

# ── --exists ──────────────────────────────────────────────────────────────────

test_expect_success '--exists succeeds for existing ref' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/main
	)
'

test_expect_success '--exists fails for nonexistent ref' '
	(
	cd repo &&
	test_must_fail grit show-ref --exists refs/heads/nonexistent
	)
'

test_expect_success '--exists produces no output' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/main >actual &&
	test_must_be_empty actual
	)
'

# ── --tags / --branches ──────────────────────────────────────────────────────

test_expect_success '--tags shows only tag refs' '
	(
	cd repo &&
	grit show-ref --tags >actual &&
	grep refs/tags actual &&
	! grep refs/heads actual
	)
'

test_expect_success '--branches shows only branch refs' '
	(
	cd repo &&
	grit show-ref --branches >actual &&
	grep refs/heads actual &&
	! grep refs/tags actual
	)
'

test_expect_success '--tags combined with --hash' '
	(
	cd repo &&
	grit show-ref --tags --hash >actual &&
	! grep " " actual &&
	lines=$(wc -l <actual) &&
	test "$lines" -ge 3
	)
'

test_expect_success '--branches combined with --hash' '
	(
	cd repo &&
	grit show-ref --branches --hash >actual &&
	! grep " " actual &&
	test_line_count = 3 actual
	)
'

# ── --head ────────────────────────────────────────────────────────────────────

test_expect_success '--head includes HEAD in output' '
	(
	cd repo &&
	grit show-ref --head >actual &&
	grep "^[0-9a-f]* HEAD$" actual
	)
'

test_expect_success '--head shows HEAD pointing to same as main' '
	(
	cd repo &&
	C=$(cat "$TRASH_DIRECTORY/oid_C") &&
	grit show-ref --head >actual &&
	grep "^$C HEAD$" actual
	)
'

test_expect_success '--head --hash includes HEAD hash' '
	(
	cd repo &&
	C=$(cat "$TRASH_DIRECTORY/oid_C") &&
	grit show-ref --head --hash >actual &&
	grep "$C" actual
	)
'

# ── combined flags ────────────────────────────────────────────────────────────

test_expect_success '--verify --hash shows only SHA for exact ref' '
	(
	cd repo &&
	C=$(cat "$TRASH_DIRECTORY/oid_C") &&
	grit show-ref --verify --hash refs/heads/main >actual &&
	echo "$C" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--tags --quiet succeeds for existing tag' '
	(
	cd repo &&
	grit show-ref --tags --quiet v1.0
	)
'

test_expect_success '--branches --quiet fails for nonexistent branch' '
	(
	cd repo &&
	test_must_fail grit show-ref --branches --quiet nonexistent
	)
'

test_done
