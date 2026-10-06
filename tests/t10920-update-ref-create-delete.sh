#!/bin/sh
# Tests for update-ref create, delete, and verify operations.

test_description='update-ref create and delete refs'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&

	EMPTY_TREE=$(printf "" | grit hash-object -w -t tree --stdin) &&

	C1=$(grit commit-tree "$EMPTY_TREE" -m "one") &&
	C2=$(grit commit-tree "$EMPTY_TREE" -p "$C1" -m "two") &&
	C3=$(grit commit-tree "$EMPTY_TREE" -p "$C2" -m "three") &&
	C4=$(grit commit-tree "$EMPTY_TREE" -p "$C3" -m "four") &&

	echo "$C1" >"$TRASH_DIRECTORY/oid_C1" &&
	echo "$C2" >"$TRASH_DIRECTORY/oid_C2" &&
	echo "$C3" >"$TRASH_DIRECTORY/oid_C3" &&
	echo "$C4" >"$TRASH_DIRECTORY/oid_C4"
	)
'

# ── Basic create ─────────────────────────────────────────────────────────────

test_expect_success 'create a branch ref' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	grit update-ref refs/heads/new-branch "$C1" &&
	grit show-ref --verify refs/heads/new-branch >actual &&
	grep "$C1" actual
	)
'

test_expect_success 'create a tag ref' '
	(
	cd repo &&
	C2=$(cat "$TRASH_DIRECTORY/oid_C2") &&
	grit update-ref refs/tags/new-tag "$C2" &&
	grit show-ref --verify refs/tags/new-tag >actual &&
	grep "$C2" actual
	)
'

test_expect_success 'create a custom namespace ref' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	grit update-ref refs/custom/myref "$C1" &&
	grit show-ref --verify refs/custom/myref >actual &&
	grep "$C1" actual
	)
'

test_expect_success 'create deeply nested ref' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_C3") &&
	grit update-ref refs/heads/a/b/c/d "$C3" &&
	grit show-ref --verify refs/heads/a/b/c/d >actual &&
	grep "$C3" actual
	)
'

# ── Update existing ──────────────────────────────────────────────────────────

test_expect_success 'update ref to new value' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	C2=$(cat "$TRASH_DIRECTORY/oid_C2") &&
	grit update-ref refs/heads/update-me "$C1" &&
	grit update-ref refs/heads/update-me "$C2" &&
	grit show-ref --verify refs/heads/update-me >actual &&
	grep "$C2" actual
	)
'

test_expect_success 'update ref with old-value check succeeds' '
	(
	cd repo &&
	C2=$(cat "$TRASH_DIRECTORY/oid_C2") &&
	C3=$(cat "$TRASH_DIRECTORY/oid_C3") &&
	grit update-ref refs/heads/update-me "$C3" "$C2" &&
	grit show-ref --verify refs/heads/update-me >actual &&
	grep "$C3" actual
	)
'

test_expect_success 'update ref with wrong old-value fails' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	C4=$(cat "$TRASH_DIRECTORY/oid_C4") &&
	test_must_fail grit update-ref refs/heads/update-me "$C4" "$C1" 2>err
	)
'

test_expect_success 'ref unchanged after failed update' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_C3") &&
	grit show-ref --verify refs/heads/update-me >actual &&
	grep "$C3" actual
	)
'

# ── Delete ───────────────────────────────────────────────────────────────────

test_expect_success 'delete ref with -d' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	grit update-ref refs/heads/to-delete "$C1" &&
	grit show-ref --exists refs/heads/to-delete &&
	grit update-ref -d refs/heads/to-delete &&
	test_must_fail grit show-ref --exists refs/heads/to-delete
	)
'

test_expect_success 'delete ref with old-value check' '
	(
	cd repo &&
	C2=$(cat "$TRASH_DIRECTORY/oid_C2") &&
	grit update-ref refs/heads/del-checked "$C2" &&
	grit update-ref -d refs/heads/del-checked "$C2" &&
	test_must_fail grit show-ref --exists refs/heads/del-checked
	)
'

test_expect_success 'delete ref with wrong old-value fails' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_C3") &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	grit update-ref refs/heads/del-fail "$C3" &&
	test_must_fail grit update-ref -d refs/heads/del-fail "$C1" 2>err &&
	grit show-ref --exists refs/heads/del-fail
	)
'

test_expect_success 'delete nonexistent ref is a no-op' '
	(
	cd repo &&
	grit update-ref -d refs/heads/nonexistent 2>err &&
	test_must_fail grit show-ref --exists refs/heads/nonexistent
	)
'

test_expect_success 'delete tag ref' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	grit update-ref refs/tags/temp-tag "$C1" &&
	grit update-ref -d refs/tags/temp-tag &&
	test_must_fail grit show-ref --exists refs/tags/temp-tag
	)
'

test_expect_success 'delete deeply nested ref' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	grit update-ref refs/heads/x/y/z "$C1" &&
	grit update-ref -d refs/heads/x/y/z &&
	test_must_fail grit show-ref --exists refs/heads/x/y/z
	)
'

# ── Create with zero old-value ───────────────────────────────────────────────

test_expect_success 'create new ref with zero old-value succeeds' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	ZERO="0000000000000000000000000000000000000000" &&
	grit update-ref refs/heads/from-zero "$C1" "$ZERO" &&
	grit show-ref --exists refs/heads/from-zero
	)
'

test_expect_success 'create existing ref with zero old-value fails' '
	(
	cd repo &&
	C2=$(cat "$TRASH_DIRECTORY/oid_C2") &&
	ZERO="0000000000000000000000000000000000000000" &&
	test_must_fail grit update-ref refs/heads/from-zero "$C2" "$ZERO" 2>err
	)
'

# ── Multiple refs ────────────────────────────────────────────────────────────

test_expect_success 'create multiple refs in sequence' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	C2=$(cat "$TRASH_DIRECTORY/oid_C2") &&
	C3=$(cat "$TRASH_DIRECTORY/oid_C3") &&
	C4=$(cat "$TRASH_DIRECTORY/oid_C4") &&
	grit update-ref refs/heads/multi-a "$C1" &&
	grit update-ref refs/heads/multi-b "$C2" &&
	grit update-ref refs/heads/multi-c "$C3" &&
	grit update-ref refs/heads/multi-d "$C4" &&
	grit show-ref --verify refs/heads/multi-a >actual_a &&
	grit show-ref --verify refs/heads/multi-b >actual_b &&
	grit show-ref --verify refs/heads/multi-c >actual_c &&
	grit show-ref --verify refs/heads/multi-d >actual_d &&
	grep "$C1" actual_a &&
	grep "$C2" actual_b &&
	grep "$C3" actual_c &&
	grep "$C4" actual_d
	)
'

test_expect_success 'update all multi refs to same value' '
	(
	cd repo &&
	C4=$(cat "$TRASH_DIRECTORY/oid_C4") &&
	grit update-ref refs/heads/multi-a "$C4" &&
	grit update-ref refs/heads/multi-b "$C4" &&
	grit update-ref refs/heads/multi-c "$C4" &&
	grit show-ref --hash refs/heads/multi-a >actual_a &&
	grit show-ref --hash refs/heads/multi-b >actual_b &&
	grit show-ref --hash refs/heads/multi-c >actual_c &&
	echo "$C4" >expect &&
	test_cmp expect actual_a &&
	test_cmp expect actual_b &&
	test_cmp expect actual_c
	)
'

# ── Ref naming edge cases ───────────────────────────────────────────────────

test_expect_success 'ref with hyphen in name' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	grit update-ref refs/heads/my-branch-name "$C1" &&
	grit show-ref --exists refs/heads/my-branch-name
	)
'

test_expect_success 'ref with dots in name' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	grit update-ref refs/heads/release-1.2.3 "$C1" &&
	grit show-ref --exists refs/heads/release-1.2.3
	)
'

test_expect_success 'ref with underscore in name' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	grit update-ref refs/heads/my_branch "$C1" &&
	grit show-ref --exists refs/heads/my_branch
	)
'

# ── Notes and stash refs ────────────────────────────────────────────────────

test_expect_success 'create notes ref' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	grit update-ref refs/notes/commits "$C1" &&
	grit show-ref --verify refs/notes/commits >actual &&
	grep "$C1" actual
	)
'

test_expect_success 'create stash ref' '
	(
	cd repo &&
	C2=$(cat "$TRASH_DIRECTORY/oid_C2") &&
	grit update-ref refs/stash "$C2" &&
	grit show-ref --verify refs/stash >actual &&
	grep "$C2" actual
	)
'

# ── --no-deref ───────────────────────────────────────────────────────────────

test_expect_success 'update-ref --no-deref on symbolic ref updates symref itself' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	C2=$(cat "$TRASH_DIRECTORY/oid_C2") &&
	grit update-ref refs/heads/sym-target "$C1" &&
	grit symbolic-ref refs/heads/sym-link refs/heads/sym-target &&
	grit update-ref --no-deref refs/heads/sym-link "$C2" &&
	# sym-link should now be a regular ref pointing to C2
	grit show-ref --verify refs/heads/sym-link >actual &&
	grep "$C2" actual
	)
'

# ── In empty repo ───────────────────────────────────────────────────────────

test_expect_success 'create first ref in empty repo' '
	(
	grit init empty &&
	cd empty &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	EMPTY_TREE=$(printf "" | grit hash-object -w -t tree --stdin) &&
	C=$(grit commit-tree "$EMPTY_TREE" -m "init") &&
	grit update-ref refs/heads/main "$C" &&
	grit show-ref --verify refs/heads/main >actual &&
	grep "$C" actual
	)
'

test_expect_success 'delete only ref in repo' '
	(
	cd empty &&
	grit update-ref -d refs/heads/main &&
	test_must_fail grit show-ref 2>/dev/null
	)
'

# ── Rapid create-delete cycle ────────────────────────────────────────────────

test_expect_success 'rapid create-delete cycle' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	grit update-ref refs/heads/rapid "$C1" &&
	grit update-ref -d refs/heads/rapid &&
	grit update-ref refs/heads/rapid "$C1" &&
	grit update-ref -d refs/heads/rapid &&
	grit update-ref refs/heads/rapid "$C1" &&
	grit show-ref --exists refs/heads/rapid
	)
'

test_expect_success 'create-update-delete in one sequence' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	C2=$(cat "$TRASH_DIRECTORY/oid_C2") &&
	C3=$(cat "$TRASH_DIRECTORY/oid_C3") &&
	grit update-ref refs/heads/lifecycle "$C1" &&
	grit update-ref refs/heads/lifecycle "$C2" "$C1" &&
	grit update-ref refs/heads/lifecycle "$C3" "$C2" &&
	grit update-ref -d refs/heads/lifecycle "$C3" &&
	test_must_fail grit show-ref --exists refs/heads/lifecycle
	)
'

test_done
