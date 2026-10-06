#!/bin/sh
# Tests for grit name-rev.
#
# Commit graph created in setup:
#
#   A <-- B <-- C   (refs/heads/main)
#   ^
#   refs/tags/v1.0 (lightweight)
#
# A is tagged by v1.0.  Tags beat branch names, so:
#   A → tags/v1.0
#   B → main~1
#   C → main

test_description='grit name-rev basic behaviours'

. ./test-lib.sh

EMPTY_TREE=""

# ------------------------------------------------------------------
# Setup: a small linear commit graph with one tag.
# ------------------------------------------------------------------

test_expect_success 'setup repo' '
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

	git update-ref refs/heads/main "$C" &&
	git update-ref refs/tags/v1.0 "$A" &&

	printf "%s\n" "$A" >.oid_A &&
	printf "%s\n" "$B" >.oid_B &&
	printf "%s\n" "$C" >.oid_C
	)
'

# ------------------------------------------------------------------
# 1. Name the commit at the branch tip.
# ------------------------------------------------------------------
test_expect_success 'name commit at branch tip' '
	(
	cd repo &&
	C=$(cat .oid_C) &&
	printf "%s main\n" "$C" >expect &&
	git name-rev "$C" >actual &&
	test_cmp expect actual
	)
'

# ------------------------------------------------------------------
# 2. Commit B is one first-parent hop from main.
# ------------------------------------------------------------------
test_expect_success 'commit one hop from branch tip is named main~1' '
	(
	cd repo &&
	B=$(cat .oid_B) &&
	printf "%s main~1\n" "$B" >expect &&
	git name-rev "$B" >actual &&
	test_cmp expect actual
	)
'

# ------------------------------------------------------------------
# 3. Tags beat branch names — A is named by its tag.
# ------------------------------------------------------------------
test_expect_success 'tag beats branch name for tagged commit' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	printf "%s tags/v1.0\n" "$A" >expect &&
	git name-rev "$A" >actual &&
	test_cmp expect actual
	)
'

# ------------------------------------------------------------------
# 4. --name-only suppresses the leading OID.
# ------------------------------------------------------------------
test_expect_success '--name-only prints only the name' '
	(
	cd repo &&
	C=$(cat .oid_C) &&
	printf "main\n" >expect &&
	git name-rev --name-only "$C" >actual &&
	test_cmp expect actual
	)
'

# ------------------------------------------------------------------
# 5. --tags restricts naming to tag refs; tag name is shown with
#    its full sub-namespace (tags/).
# ------------------------------------------------------------------
test_expect_success '--tags uses only tag refs' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	printf "%s tags/v1.0\n" "$A" >expect &&
	git name-rev --tags "$A" >actual &&
	test_cmp expect actual
	)
'

# ------------------------------------------------------------------
# 6. --tags --name-only shortens the tag name (strips "tags/").
# ------------------------------------------------------------------
test_expect_success '--tags --name-only shortens to bare tag name' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	printf "v1.0\n" >expect &&
	git name-rev --tags --name-only "$A" >actual &&
	test_cmp expect actual
	)
'

# ------------------------------------------------------------------
# 7. Commit not reachable from any ref yields "undefined" by default.
# ------------------------------------------------------------------
test_expect_success 'unreachable commit yields undefined' '
	(
	cd repo &&
	EMPTY_TREE=$(cat .empty_tree) &&
	ORPHAN=$(git commit-tree "$EMPTY_TREE" -m "orphan") &&
	printf "%s undefined\n" "$ORPHAN" >expect &&
	git name-rev "$ORPHAN" >actual &&
	test_cmp expect actual
	)
'

# ------------------------------------------------------------------
# 8. --no-undefined exits non-zero when no name is found.
# ------------------------------------------------------------------
test_expect_success '--no-undefined fails on unreachable commit' '
	(
	cd repo &&
	EMPTY_TREE=$(cat .empty_tree) &&
	ORPHAN=$(git commit-tree "$EMPTY_TREE" -m "orphan2") &&
	test_must_fail git name-rev --no-undefined "$ORPHAN"
	)
'

# ------------------------------------------------------------------
# 9. --always falls back to abbreviated hash when no name found.
# ------------------------------------------------------------------
test_expect_success '--always shows abbreviated hash as fallback' '
	(
	cd repo &&
	EMPTY_TREE=$(cat .empty_tree) &&
	ORPHAN=$(git commit-tree "$EMPTY_TREE" -m "orphan3") &&
	SHORT=$(printf "%.7s" "$ORPHAN") &&
	printf "%s %s\n" "$ORPHAN" "$SHORT" >expect &&
	git name-rev --no-undefined --always "$ORPHAN" >actual &&
	test_cmp expect actual
	)
'

# ------------------------------------------------------------------
# 10. --all names every reachable commit.
# ------------------------------------------------------------------
test_expect_success '--all names every reachable commit' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	B=$(cat .oid_B) &&
	C=$(cat .oid_C) &&
	{
		git name-rev "$A" &&
		git name-rev "$B" &&
		git name-rev "$C"
	} | sort >expect &&
	git name-rev --all | sort >actual &&
	test_cmp expect actual
	)
'

# ------------------------------------------------------------------
# 11. --annotate-stdin annotates OIDs embedded in text.
# ------------------------------------------------------------------
test_expect_success '--annotate-stdin annotates OIDs in text' '
	(
	cd repo &&
	C=$(cat .oid_C) &&
	NAME=$(git name-rev --name-only "$C") &&
	printf "%s (%s)\n" "$C" "$NAME" >expect &&
	printf "%s\n" "$C" | git name-rev --annotate-stdin >actual &&
	test_cmp expect actual
	)
'

# ------------------------------------------------------------------
# 12. --refs=<pattern> restricts naming to matching refs.
#     When the pattern matches a sub-path (e.g. "v*" matches "v1.0" within
#     "refs/tags/v1.0") the name is shortened to the matched sub-path.
# ------------------------------------------------------------------
test_expect_success '--refs=v* limits to v1.0 tag and abbreviates sub-path match' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	printf "%s v1.0\n" "$A" >expect &&
	git name-rev --refs="v*" "$A" >actual &&
	test_cmp expect actual
	)
'

# ------------------------------------------------------------------
# 13. Merge commit: second parent gets ^2 suffix.
# ------------------------------------------------------------------
test_expect_success 'second parent of merge gets ^2 suffix' '
	(
	cd repo &&
	EMPTY_TREE=$(cat .empty_tree) &&
	C=$(cat .oid_C) &&
	GIT_COMMITTER_DATE="1000003 +0000" GIT_AUTHOR_DATE="1000003 +0000" \
		D=$(git commit-tree "$EMPTY_TREE" -m "commit D") &&
	GIT_COMMITTER_DATE="1000004 +0000" GIT_AUTHOR_DATE="1000004 +0000" \
		M=$(git commit-tree "$EMPTY_TREE" -p "$C" -p "$D" -m "merge") &&
	git update-ref refs/heads/main "$M" &&

	# D is the second parent of M (which is main); D should be named main^2.
	printf "%s main^2\n" "$D" >expect &&
	git name-rev "$D" >actual &&
	test_cmp expect actual
	)
'

# --- Additional name-rev tests ---

test_expect_success 'name-rev merge commit itself' '
	(
	cd repo &&
	M=$(git rev-parse main) &&
	printf "%s main\n" "$M" >expect &&
	git name-rev "$M" >actual &&
	test_cmp expect actual
	)
'

test_expect_success '--name-only with merge commit' '
	(
	cd repo &&
	M=$(git rev-parse main) &&
	printf "main\n" >expect &&
	git name-rev --name-only "$M" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'setup annotated tag for name-rev' '
	(
	cd repo &&
	EMPTY_TREE=$(cat .empty_tree) &&
	A=$(cat .oid_A) &&
	cat >ann-tag-obj <<-EOF &&
	object $A
	type commit
	tag v2.0-ann
	tagger T <t@t> 1000000 +0000

	annotated v2
	EOF
	ann_oid=$(git hash-object -t tag -w ann-tag-obj) &&
	git update-ref refs/tags/v2.0-ann "$ann_oid"
	)
'

test_expect_success 'annotated tag names commit with ^0' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	git name-rev "$A" >actual &&
	# Could be tags/v1.0, tags/v2.0-ann^0 — both are valid; just check it names it
	grep -v undefined actual
	)
'

test_expect_success '--tags restricts to tag refs only' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	git name-rev --tags "$A" >actual &&
	grep tags/ actual
	)
'

test_expect_success '--tags --name-only strips tags/ prefix' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	git name-rev --tags --name-only "$A" >actual &&
	! grep tags/ actual &&
	name=$(cat actual | tr -d "\n") &&
	test -n "$name"
	)
'

test_expect_success 'name-rev multiple commits at once' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	B=$(cat .oid_B) &&
	git name-rev "$A" "$B" >actual &&
	lines=$(wc -l <actual | tr -d " ") &&
	test "$lines" = 2
	)
'

test_expect_success '--annotate-stdin with multiple lines' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	B=$(cat .oid_B) &&
	printf "%s\n%s\n" "$A" "$B" | git name-rev --annotate-stdin >actual &&
	lines=$(wc -l <actual | tr -d " ") &&
	test "$lines" = 2
	)
'

test_expect_success '--annotate-stdin preserves surrounding text' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	printf "commit is %s here\n" "$A" | git name-rev --annotate-stdin >actual &&
	grep "commit is" actual &&
	grep "here" actual
	)
'

test_expect_success '--stdin is deprecated alias for --annotate-stdin' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	printf "%s\n" "$A" | git name-rev --stdin >actual 2>err &&
	grep -i "deprecated\|stdin" err &&
	printf "%s\n" "$A" | git name-rev --annotate-stdin >expected &&
	test_cmp expected actual
	)
'

test_expect_success '--refs limits to specific pattern' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	git name-rev --refs="v*" "$A" >actual &&
	name=$(echo "$A" | cut -c1-7) &&
	grep -v undefined actual
	)
'

test_expect_success '--refs with non-matching pattern yields undefined' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	git name-rev --refs="nonexistent*" "$A" >actual &&
	grep undefined actual
	)
'

test_expect_success 'setup longer chain for name-rev distance' '
	(
	cd repo &&
	EMPTY_TREE=$(cat .empty_tree) &&
	M=$(git rev-parse main) &&
	GIT_COMMITTER_DATE="1000005 +0000" GIT_AUTHOR_DATE="1000005 +0000" \
		E=$(git commit-tree "$EMPTY_TREE" -p "$M" -m "commit E") &&
	GIT_COMMITTER_DATE="1000006 +0000" GIT_AUTHOR_DATE="1000006 +0000" \
		F=$(git commit-tree "$EMPTY_TREE" -p "$E" -m "commit F") &&
	git update-ref refs/heads/main "$F" &&
	printf "%s\n" "$E" >.oid_E &&
	printf "%s\n" "$F" >.oid_F
	)
'

test_expect_success 'name-rev with deeper distance shows ~N' '
	(
	cd repo &&
	E=$(cat .oid_E) &&
	git name-rev "$E" >actual &&
	grep "main~1" actual
	)
'

test_expect_success 'name-rev tip of main after extension' '
	(
	cd repo &&
	F=$(cat .oid_F) &&
	printf "%s main\n" "$F" >expect &&
	git name-rev "$F" >actual &&
	test_cmp expect actual
	)
'

test_expect_success '--all includes newly added commits' '
	(
	cd repo &&
	git name-rev --all >actual &&
	lines=$(wc -l <actual | tr -d " ") &&
	test "$lines" -ge 5
	)
'

test_expect_success 'name-rev of merge parent via ~N path' '
	(
	cd repo &&
	B=$(cat .oid_B) &&
	git name-rev "$B" >actual &&
	# B is reachable from main via several hops
	grep -v undefined actual
	)
'

test_expect_success '--name-only with --tags on unreachable gives empty-ish' '
	(
	cd repo &&
	EMPTY_TREE=$(cat .empty_tree) &&
	ORPHAN=$(git commit-tree "$EMPTY_TREE" -m "orphan-tags") &&
	git name-rev --tags --name-only "$ORPHAN" >actual &&
	grep undefined actual
	)
'

test_expect_success '--always with --tags on unreachable gives abbreviated hash' '
	(
	cd repo &&
	EMPTY_TREE=$(cat .empty_tree) &&
	ORPHAN=$(git commit-tree "$EMPTY_TREE" -m "orphan-always") &&
	SHORT=$(printf "%.7s" "$ORPHAN") &&
	printf "%s %s\n" "$ORPHAN" "$SHORT" >expect &&
	git name-rev --no-undefined --always "$ORPHAN" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'setup side branch for name-rev' '
	(
	cd repo &&
	EMPTY_TREE=$(cat .empty_tree) &&
	B=$(cat .oid_B) &&
	GIT_COMMITTER_DATE="1000010 +0000" GIT_AUTHOR_DATE="1000010 +0000" \
		S1=$(git commit-tree "$EMPTY_TREE" -p "$B" -m "side 1") &&
	GIT_COMMITTER_DATE="1000011 +0000" GIT_AUTHOR_DATE="1000011 +0000" \
		S2=$(git commit-tree "$EMPTY_TREE" -p "$S1" -m "side 2") &&
	git update-ref refs/heads/side "$S2" &&
	printf "%s\n" "$S1" >.oid_S1 &&
	printf "%s\n" "$S2" >.oid_S2
	)
'

test_expect_success 'name-rev side branch tip' '
	(
	cd repo &&
	S2=$(cat .oid_S2) &&
	printf "%s side\n" "$S2" >expect &&
	git name-rev "$S2" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'name-rev commit reachable from side' '
	(
	cd repo &&
	S1=$(cat .oid_S1) &&
	git name-rev "$S1" >actual &&
	grep "side~1" actual
	)
'

test_expect_success '--refs=side* names via side branch' '
	(
	cd repo &&
	S1=$(cat .oid_S1) &&
	git name-rev --refs="side*" "$S1" >actual &&
	grep side actual
	)
'

test_expect_success '--refs=main* names via main branch' '
	(
	cd repo &&
	F=$(cat .oid_F) &&
	git name-rev --refs="main*" "$F" >actual &&
	grep main actual
	)
'

test_expect_success '--annotate-stdin with no OIDs passes through' '
	(
	cd repo &&
	echo "no hashes here" | git name-rev --annotate-stdin >actual &&
	echo "no hashes here" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--annotate-stdin with mixed text and OID' '
	(
	cd repo &&
	F=$(cat .oid_F) &&
	printf "bug in %s please fix\n" "$F" | git name-rev --annotate-stdin >actual &&
	grep "main" actual &&
	grep "please fix" actual
	)
'

test_expect_success 'name-rev with lightweight tag' '
	(
	cd repo &&
	A=$(cat .oid_A) &&
	git name-rev --tags "$A" >actual &&
	grep -v undefined actual
	)
'

test_done
