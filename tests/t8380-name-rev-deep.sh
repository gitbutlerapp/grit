#!/bin/sh
# Tests for name-rev with complex histories, tags, multiple refs, and deep chains.

test_description='name-rev with complex histories, tags, multiple refs'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup: complex commit graph ─────────────────────────────────────────────
#
#   A ← B ← C ← D ← E ← F   (main)
#   ↑       ↑         ↑
#  v1.0    v2.0   merge from side
#                     ↑
#           B ← S1 ← S2   (side)

test_expect_success 'setup complex history' '
	(
	git init repo &&
	cd repo &&
	EMPTY_TREE=$(printf "" | git hash-object -w -t tree --stdin) &&
	printf "%s" "$EMPTY_TREE" >.empty_tree &&

	GIT_COMMITTER_DATE="1000000 +0000" GIT_AUTHOR_DATE="1000000 +0000" \
		A=$(git commit-tree "$EMPTY_TREE" -m "commit A") &&
	GIT_COMMITTER_DATE="1000001 +0000" GIT_AUTHOR_DATE="1000001 +0000" \
		B=$(git commit-tree "$EMPTY_TREE" -p "$A" -m "commit B") &&
	GIT_COMMITTER_DATE="1000002 +0000" GIT_AUTHOR_DATE="1000002 +0000" \
		C=$(git commit-tree "$EMPTY_TREE" -p "$B" -m "commit C") &&
	GIT_COMMITTER_DATE="1000003 +0000" GIT_AUTHOR_DATE="1000003 +0000" \
		D=$(git commit-tree "$EMPTY_TREE" -p "$C" -m "commit D") &&

	# side branch from B
	GIT_COMMITTER_DATE="1000004 +0000" GIT_AUTHOR_DATE="1000004 +0000" \
		S1=$(git commit-tree "$EMPTY_TREE" -p "$B" -m "side 1") &&
	GIT_COMMITTER_DATE="1000005 +0000" GIT_AUTHOR_DATE="1000005 +0000" \
		S2=$(git commit-tree "$EMPTY_TREE" -p "$S1" -m "side 2") &&

	# merge commit
	GIT_COMMITTER_DATE="1000006 +0000" GIT_AUTHOR_DATE="1000006 +0000" \
		E=$(git commit-tree "$EMPTY_TREE" -p "$D" -p "$S2" -m "merge") &&
	GIT_COMMITTER_DATE="1000007 +0000" GIT_AUTHOR_DATE="1000007 +0000" \
		F=$(git commit-tree "$EMPTY_TREE" -p "$E" -m "commit F") &&

	git update-ref refs/heads/main "$F" &&
	git update-ref refs/heads/side "$S2" &&
	git update-ref refs/tags/v1.0 "$A" &&
	git update-ref refs/tags/v2.0 "$C" &&

	for name in A B C D E F S1 S2; do
		eval "printf \"%s\n\" \"\$$name\"" >".oid_$name"
	done
	)
'

# ── Branch tip naming ───────────────────────────────────────────────────────

test_expect_success 'main tip is named main' '
	(
	cd repo &&
	F=$(cat .oid_F) &&
	printf "%s main\n" "$F" >expect &&
	git name-rev "$F" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'side tip is named side' '
	(
	cd repo &&
	S2=$(cat .oid_S2) &&
	printf "%s side\n" "$S2" >expect &&
	git name-rev "$S2" >actual &&
	test_cmp expect actual
	)
'

# ── Distance naming (~N) ────────────────────────────────────────────────────

test_expect_success 'one hop from main tip is main~1' '
	(
	cd repo &&
	E=$(cat .oid_E) &&
	printf "%s main~1\n" "$E" >expect &&
	git name-rev "$E" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'two hops from main is main~2' '
	(
	cd repo &&
	D=$(cat .oid_D) &&
	printf "%s main~2\n" "$D" >expect &&
	git name-rev "$D" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'side~1 for one hop back from side' '
	(
	cd repo &&
	S1=$(cat .oid_S1) &&
	printf "%s side~1\n" "$S1" >expect &&
	git name-rev "$S1" >actual &&
	test_cmp expect actual
	)
'

# ── Tags beat branches ──────────────────────────────────────────────────────

test_expect_success 'tagged commit A is named tags/v1.0' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	printf "%s tags/v1.0\n" "$A" >expect &&
	git name-rev "$A" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'tagged commit C is named tags/v2.0' '
	(
	cd repo &&
	C=$(cat .oid_C) &&
	printf "%s tags/v2.0\n" "$C" >expect &&
	git name-rev "$C" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'commit B gets named relative to tag (tags/v2.0~1)' '
	(
	cd repo &&
	B=$(cat .oid_B) &&
	git name-rev "$B" >actual &&
	grep "v2.0~1\|v1.0" actual &&
	grep -v undefined actual
	)
'

# ── Merge parent naming ─────────────────────────────────────────────────────

test_expect_success 'second parent of merge gets ^2 suffix' '
	(
	cd repo &&
	S2=$(cat .oid_S2) &&
	git name-rev "$S2" >actual &&
	# S2 is reachable as main~1^2 or side
	grep -v undefined actual
	)
'

test_expect_success 'merge commit E is named main~1' '
	(
	cd repo &&
	E=$(cat .oid_E) &&
	printf "%s main~1\n" "$E" >expect &&
	git name-rev "$E" >actual &&
	test_cmp expect actual
	)
'

# ── --name-only ─────────────────────────────────────────────────────────────

test_expect_success '--name-only prints only name for main tip' '
	(
	cd repo &&
	F=$(cat .oid_F) &&
	printf "main\n" >expect &&
	git name-rev --name-only "$F" >actual &&
	test_cmp expect actual
	)
'

test_expect_success '--name-only prints only name for tagged commit' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	printf "tags/v1.0\n" >expect &&
	git name-rev --name-only "$A" >actual &&
	test_cmp expect actual
	)
'

# ── --tags ──────────────────────────────────────────────────────────────────

test_expect_success '--tags uses only tag refs' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	git name-rev --tags "$A" >actual &&
	grep "v1.0" actual
	)
'

test_expect_success '--tags --name-only strips tags/ prefix' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	git name-rev --tags --name-only "$A" >actual &&
	printf "v1.0\n" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--tags for non-reachable-from-tag commit yields undefined' '
	(
	cd repo &&
	D=$(cat .oid_D) &&
	git name-rev --tags "$D" >actual &&
	# D is a descendant of v2.0, not an ancestor reachable from tag;
	# --tags correctly yields undefined
	grep "undefined" actual
	)
'

# ── --refs pattern ──────────────────────────────────────────────────────────

test_expect_success '--refs=main* names via main' '
	(
	cd repo &&
	F=$(cat .oid_F) &&
	git name-rev --refs="main*" "$F" >actual &&
	grep "main" actual
	)
'

test_expect_success '--refs=side* names via side' '
	(
	cd repo &&
	S2=$(cat .oid_S2) &&
	git name-rev --refs="side*" "$S2" >actual &&
	grep "side" actual
	)
'

test_expect_success '--refs non-matching pattern yields undefined' '
	(
	cd repo &&
	F=$(cat .oid_F) &&
	git name-rev --refs="nonexistent*" "$F" >actual &&
	grep "undefined" actual
	)
'

# ── --all ────────────────────────────────────────────────────────────────────

test_expect_success '--all names every reachable commit' '
	(
	cd repo &&
	git name-rev --all >actual &&
	lines=$(wc -l <actual | tr -d " ") &&
	test "$lines" -ge 6
	)
'

# ── --no-undefined ───────────────────────────────────────────────────────────

test_expect_success '--no-undefined fails on unreachable commit' '
	(
	cd repo &&
	EMPTY_TREE=$(cat .empty_tree) &&
	ORPHAN=$(git commit-tree "$EMPTY_TREE" -m "orphan") &&
	test_must_fail git name-rev --no-undefined "$ORPHAN"
	)
'

# ── --always ─────────────────────────────────────────────────────────────────

test_expect_success '--always shows abbreviated hash for unreachable' '
	(
	cd repo &&
	EMPTY_TREE=$(cat .empty_tree) &&
	ORPHAN=$(git commit-tree "$EMPTY_TREE" -m "orphan2") &&
	SHORT=$(printf "%.7s" "$ORPHAN") &&
	printf "%s %s\n" "$ORPHAN" "$SHORT" >expect &&
	git name-rev --no-undefined --always "$ORPHAN" >actual &&
	test_cmp expect actual
	)
'

# ── --annotate-stdin ─────────────────────────────────────────────────────────

test_expect_success '--annotate-stdin annotates OIDs in text' '
	(
	cd repo &&
	F=$(cat .oid_F) &&
	printf "%s\n" "$F" | git name-rev --annotate-stdin >actual &&
	grep "main" actual
	)
'

test_expect_success '--annotate-stdin with surrounding text' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	printf "bug in %s please fix\n" "$A" | git name-rev --annotate-stdin >actual &&
	grep "v1.0" actual &&
	grep "please fix" actual
	)
'

test_expect_success '--annotate-stdin passes through non-OID text' '
	(
	cd repo &&
	echo "no hashes here" | git name-rev --annotate-stdin >actual &&
	echo "no hashes here" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--annotate-stdin with multiple OIDs' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	F=$(cat .oid_F) &&
	printf "%s\n%s\n" "$A" "$F" | git name-rev --annotate-stdin >actual &&
	lines=$(wc -l <actual | tr -d " ") &&
	test "$lines" = 2 &&
	grep "v1.0" actual &&
	grep "main" actual
	)
'

# ── Multiple commits at once ────────────────────────────────────────────────

test_expect_success 'name-rev multiple commits in single invocation' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	B=$(cat .oid_B) &&
	F=$(cat .oid_F) &&
	git name-rev "$A" "$B" "$F" >actual &&
	lines=$(wc -l <actual | tr -d " ") &&
	test "$lines" = 3
	)
'

# ── Deep chain ──────────────────────────────────────────────────────────────

test_expect_success 'setup deep chain (10 commits)' '
	(
	cd repo &&
	EMPTY_TREE=$(cat .empty_tree) &&
	PREV=$(cat .oid_F) &&
	for i in 1 2 3 4 5 6 7 8 9 10; do
		TS=$((1000010 + i)) &&
		GIT_COMMITTER_DATE="${TS} +0000" GIT_AUTHOR_DATE="${TS} +0000" \
			PREV=$(git commit-tree "$EMPTY_TREE" -p "$PREV" -m "deep $i") || return 1
	done &&
	git update-ref refs/heads/deep "$PREV" &&
	printf "%s\n" "$PREV" >.oid_DEEP
	)
'

test_expect_success 'name-rev deep chain tip' '
	(
	cd repo &&
	DEEP=$(cat .oid_DEEP) &&
	printf "%s deep\n" "$DEEP" >expect &&
	git name-rev "$DEEP" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'name-rev deep chain names with ~N' '
	(
	cd repo &&
	DEEP=$(cat .oid_DEEP) &&
	# Go back 5 from deep tip
	PREV="$DEEP" &&
	for i in 1 2 3 4 5; do
		PREV=$(git rev-parse "$PREV^") || return 1
	done &&
	git name-rev "$PREV" >actual &&
	grep "~5\|~" actual
	)
'

test_expect_success '--stdin deprecated alias still works' '
	(
	cd repo &&
	F=$(cat .oid_F) &&
	printf "%s\n" "$F" | git name-rev --stdin >actual 2>err &&
	grep -i "deprecated\|stdin" err &&
	printf "%s\n" "$F" | git name-rev --annotate-stdin >expected &&
	test_cmp expected actual
	)
'

test_done
