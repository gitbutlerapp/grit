#!/bin/sh
# Tests for show-ref --dereference (-d), --hash, --abbrev, --head,
# --tags, --branches, --verify, --quiet, and their combinations.

test_description='show-ref dereference and display options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with branches and annotated tags' '
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
	grit update-ref refs/heads/feature "$B" &&
	grit update-ref refs/heads/old "$A" &&

	git symbolic-ref HEAD refs/heads/main &&

	grit tag -a -m "version 1" v1.0 "$A" &&
	grit tag -a -m "version 2" v2.0 "$B" &&
	grit tag lightweight "$C" &&

	echo "$A" >"$TRASH_DIRECTORY/oid_A" &&
	echo "$B" >"$TRASH_DIRECTORY/oid_B" &&
	echo "$C" >"$TRASH_DIRECTORY/oid_C"
	)
'

# ── basic show-ref ────────────────────────────────────────────────────────────

test_expect_success 'show-ref lists all refs' '
	(
	cd repo &&
	grit show-ref >actual &&
	test_line_count = 6 actual
	)
'

test_expect_success 'show-ref output format is SHA<space>refname' '
	(
	cd repo &&
	grit show-ref refs/heads/main >actual &&
	C=$(cat "$TRASH_DIRECTORY/oid_C") &&
	echo "$C refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref with pattern matches trailing component' '
	(
	cd repo &&
	grit show-ref main >actual &&
	test_line_count = 1 actual &&
	grep refs/heads/main actual
	)
'

test_expect_success 'show-ref with nonexistent pattern fails' '
	(
	cd repo &&
	test_must_fail grit show-ref nonexistent >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'show-ref with full refname matches exactly' '
	(
	cd repo &&
	grit show-ref refs/tags/v1.0 >actual &&
	test_line_count = 1 actual
	)
'

# ── --dereference (-d) ────────────────────────────────────────────────────────

test_expect_success '-d shows peeled entries for annotated tags' '
	(
	cd repo &&
	grit show-ref -d v1.0 >actual &&
	test_line_count = 2 actual &&
	grep "refs/tags/v1.0$" actual &&
	grep "refs/tags/v1.0\\^{}$" actual
	)
'

test_expect_success '-d peeled entry points to underlying commit' '
	(
	cd repo &&
	A=$(cat "$TRASH_DIRECTORY/oid_A") &&
	grit show-ref -d v1.0 >actual &&
	grep "^$A refs/tags/v1.0\\^{}$" actual
	)
'

test_expect_success '-d does not add peel line for lightweight tag' '
	(
	cd repo &&
	grit show-ref -d lightweight >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success '-d with no pattern shows all refs plus peeled' '
	(
	cd repo &&
	grit show-ref -d >actual &&
	grep "\\^{}$" actual >peeled &&
	test_line_count = 2 peeled
	)
'

test_expect_success '-d peeled SHA differs from tag object SHA' '
	(
	cd repo &&
	grit show-ref -d v1.0 >actual &&
	tag_sha=$(grep "refs/tags/v1.0$" actual | cut -d" " -f1) &&
	peel_sha=$(grep "\\^{}$" actual | cut -d" " -f1) &&
	test "$tag_sha" != "$peel_sha"
	)
'

test_expect_success '-d with branch pattern shows no peeled entries' '
	(
	cd repo &&
	grit show-ref -d refs/heads/main >actual &&
	test_line_count = 1 actual &&
	! grep "\\^{}" actual
	)
'

# ── --hash ────────────────────────────────────────────────────────────────────

test_expect_success '--hash shows only object IDs' '
	(
	cd repo &&
	grit show-ref --hash main >actual &&
	C=$(cat "$TRASH_DIRECTORY/oid_C") &&
	echo "$C" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--hash with pattern matching multiple refs' '
	(
	cd repo &&
	grit show-ref --hash v1.0 >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success '--hash output has no refnames' '
	(
	cd repo &&
	grit show-ref --hash main >actual &&
	! grep refs/ actual
	)
'

test_expect_success '--hash=7 abbreviates to 7 characters' '
	(
	cd repo &&
	grit show-ref --hash=7 main >actual &&
	hex=$(cat actual | tr -d "\n") &&
	len=${#hex} &&
	test "$len" = "7"
	)
'

test_expect_success '--hash=12 abbreviates to 12 characters' '
	(
	cd repo &&
	grit show-ref --hash=12 main >actual &&
	hex=$(cat actual | tr -d "\n") &&
	len=${#hex} &&
	test "$len" = "12"
	)
'

test_expect_success '--hash with no abbreviation is 40 chars' '
	(
	cd repo &&
	grit show-ref --hash main >actual &&
	hex=$(cat actual | tr -d "\n") &&
	len=${#hex} &&
	test "$len" = "40"
	)
'

# ── --abbrev ──────────────────────────────────────────────────────────────────

test_expect_success '--abbrev shortens SHA in standard output' '
	(
	cd repo &&
	grit show-ref --abbrev refs/heads/main >actual &&
	sha=$(cut -d" " -f1 <actual) &&
	len=$(printf "%s" "$sha" | wc -c) &&
	test "$len" -lt 40
	)
'

test_expect_success '--abbrev=8 produces 8-char SHA' '
	(
	cd repo &&
	grit show-ref --abbrev=8 refs/heads/main >actual &&
	sha=$(cut -d" " -f1 <actual) &&
	len=${#sha} &&
	test "$len" = "8"
	)
'

test_expect_success '--abbrev still shows refname' '
	(
	cd repo &&
	grit show-ref --abbrev refs/heads/main >actual &&
	grep refs/heads/main actual
	)
'

# ── --tags / --branches ──────────────────────────────────────────────────────

test_expect_success '--tags shows only tag refs' '
	(
	cd repo &&
	grit show-ref --tags >actual &&
	test_line_count = 3 actual &&
	grep refs/tags/ actual &&
	! grep refs/heads/ actual
	)
'

test_expect_success '--branches shows only branch refs' '
	(
	cd repo &&
	grit show-ref --branches >actual &&
	test_line_count = 3 actual &&
	grep refs/heads/ actual &&
	! grep refs/tags/ actual
	)
'

test_expect_success '--tags -d shows tags with peeled entries' '
	(
	cd repo &&
	grit show-ref --tags -d >actual &&
	grep "refs/tags/v1.0$" actual &&
	grep "refs/tags/v1.0\\^{}$" actual &&
	! grep refs/heads/ actual
	)
'

# ── --head ────────────────────────────────────────────────────────────────────

test_expect_success '--head includes HEAD in listing' '
	(
	cd repo &&
	grit show-ref --head >actual &&
	grep " HEAD$" actual
	)
'

test_expect_success '--head HEAD value matches symbolic target' '
	(
	cd repo &&
	git symbolic-ref HEAD refs/heads/main &&
	grit show-ref --head >actual &&
	head_sha=$(grep " HEAD$" actual | cut -d" " -f1) &&
	main_sha=$(grep " refs/heads/main$" actual | cut -d" " -f1) &&
	test "$head_sha" = "$main_sha"
	)
'

test_expect_success '--head adds exactly one extra line' '
	(
	cd repo &&
	grit show-ref >without_head &&
	grit show-ref --head >with_head &&
	without=$(wc -l <without_head | tr -d "[:space:]") &&
	with=$(wc -l <with_head | tr -d "[:space:]") &&
	test "$with" = "$(($without + 1))"
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

test_expect_success '--verify with short name fails' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify main
	)
'

test_expect_success '--verify with nonexistent ref fails' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify refs/heads/nonexistent
	)
'

test_expect_success '--verify --hash shows only hash' '
	(
	cd repo &&
	C=$(cat "$TRASH_DIRECTORY/oid_C") &&
	grit show-ref --verify --hash refs/heads/main >actual &&
	echo "$C" >expect &&
	test_cmp expect actual
	)
'

# ── --quiet ───────────────────────────────────────────────────────────────────

test_expect_success '-q produces no output on success' '
	(
	cd repo &&
	grit show-ref -q main >actual &&
	test_must_be_empty actual
	)
'

test_expect_success '-q exits non-zero for missing ref' '
	(
	cd repo &&
	test_must_fail grit show-ref -q nonexistent
	)
'

# ── combinations ──────────────────────────────────────────────────────────────

test_expect_success '--hash -d shows peeled hashes' '
	(
	cd repo &&
	A=$(cat "$TRASH_DIRECTORY/oid_A") &&
	grit show-ref --hash -d v1.0 >actual &&
	test_line_count = 2 actual &&
	grep "$A" actual
	)
'

test_expect_success '--head --hash shows HEAD hash first' '
	(
	cd repo &&
	grit show-ref --head --hash >actual &&
	C=$(cat "$TRASH_DIRECTORY/oid_C") &&
	head -1 actual >first &&
	echo "$C" >expect &&
	test_cmp expect first
	)
'

test_expect_success '-d --abbrev=10 abbreviates peeled entries' '
	(
	cd repo &&
	grit show-ref -d --abbrev=10 v1.0 >actual &&
	grep "\\^{}$" actual >peeled &&
	sha=$(cut -d" " -f1 <peeled) &&
	len=${#sha} &&
	test "$len" = "10"
	)
'

test_expect_success '--exists checks ref presence' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/main &&
	test_must_fail grit show-ref --exists refs/heads/nonexistent
	)
'

test_done
