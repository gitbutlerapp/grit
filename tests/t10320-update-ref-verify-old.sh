#!/bin/sh
# Tests for update-ref with old-value verification, -d (delete),
# --no-deref, -m (reflog message), error handling, and edge cases.

test_description='update-ref old-value verify, delete, no-deref, reflog'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup repo with commits' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&

	echo "one" >file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "first" &&

	echo "two" >>file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "second" &&

	echo "three" >>file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "third" &&

	echo "four" >>file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "fourth" &&

	OID1=$(grit rev-parse HEAD~3) &&
	OID2=$(grit rev-parse HEAD~2) &&
	OID3=$(grit rev-parse HEAD~1) &&
	OID4=$(grit rev-parse HEAD) &&

	echo "$OID1" >"$TRASH_DIRECTORY/oid1" &&
	echo "$OID2" >"$TRASH_DIRECTORY/oid2" &&
	echo "$OID3" >"$TRASH_DIRECTORY/oid3" &&
	echo "$OID4" >"$TRASH_DIRECTORY/oid4"
	)
'

# ── basic update-ref ─────────────────────────────────────────────────────────

test_expect_success 'create a new ref' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	grit update-ref refs/heads/new-branch "$oid1" &&
	grit rev-parse refs/heads/new-branch >actual &&
	echo "$oid1" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update an existing ref to new value' '
	(
	cd repo &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	grit update-ref refs/heads/new-branch "$oid2" &&
	grit rev-parse refs/heads/new-branch >actual &&
	echo "$oid2" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'create ref in nested namespace' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	grit update-ref refs/custom/deep/nested "$oid1" &&
	grit rev-parse refs/custom/deep/nested >actual &&
	echo "$oid1" >expect &&
	test_cmp expect actual
	)
'

# ── old-value verification ───────────────────────────────────────────────────

test_expect_success 'update with correct old value succeeds' '
	(
	cd repo &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	oid3=$(cat "$TRASH_DIRECTORY/oid3") &&
	grit update-ref refs/heads/new-branch "$oid3" "$oid2" &&
	grit rev-parse refs/heads/new-branch >actual &&
	echo "$oid3" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update with wrong old value fails' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	oid4=$(cat "$TRASH_DIRECTORY/oid4") &&
	test_must_fail grit update-ref refs/heads/new-branch "$oid4" "$oid1" 2>err &&
	test -s err
	)
'

test_expect_success 'ref unchanged after failed old-value check' '
	(
	cd repo &&
	oid3=$(cat "$TRASH_DIRECTORY/oid3") &&
	grit rev-parse refs/heads/new-branch >actual &&
	echo "$oid3" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'old value 0{40} means ref must not exist' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	ZERO=$(printf "%040d" 0) &&
	grit update-ref refs/heads/brand-new "$oid1" "$ZERO" &&
	grit rev-parse refs/heads/brand-new >actual &&
	echo "$oid1" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'old value 0{40} fails when ref exists' '
	(
	cd repo &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	ZERO=$(printf "%040d" 0) &&
	test_must_fail grit update-ref refs/heads/brand-new "$oid2" "$ZERO" 2>err &&
	test -s err
	)
'

# ── delete (-d) ──────────────────────────────────────────────────────────────

test_expect_success 'delete a ref with -d' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	grit update-ref refs/heads/to-delete "$oid1" &&
	grit update-ref -d refs/heads/to-delete &&
	test_must_fail grit rev-parse refs/heads/to-delete 2>err
	)
'

test_expect_success 'delete with correct old value' '
	(
	cd repo &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	grit update-ref refs/heads/del-verify "$oid2" &&
	grit update-ref -d refs/heads/del-verify "$oid2" &&
	test_must_fail grit rev-parse refs/heads/del-verify 2>err
	)
'

test_expect_success 'delete with wrong old value fails' '
	(
	cd repo &&
	oid3=$(cat "$TRASH_DIRECTORY/oid3") &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	grit update-ref refs/heads/del-fail "$oid3" &&
	test_must_fail grit update-ref -d refs/heads/del-fail "$oid1" 2>err &&
	test -s err &&
	grit rev-parse refs/heads/del-fail >actual &&
	echo "$oid3" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'delete nonexistent ref is idempotent (succeeds)' '
	(
	cd repo &&
	grit update-ref -d refs/heads/does-not-exist
	)
'

# ── --no-deref ───────────────────────────────────────────────────────────────

test_expect_success '--no-deref updates symbolic ref itself' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	oid4=$(cat "$TRASH_DIRECTORY/oid4") &&
	grit update-ref refs/heads/sym-target "$oid1" &&
	git symbolic-ref refs/heads/sym-link refs/heads/sym-target &&
	grit update-ref --no-deref refs/heads/sym-link "$oid4" &&
	grit rev-parse refs/heads/sym-link >actual &&
	echo "$oid4" >expect &&
	test_cmp expect actual &&
	grit rev-parse refs/heads/sym-target >target_actual &&
	echo "$oid1" >target_expect &&
	test_cmp target_expect target_actual
	)
'

test_expect_success 'without --no-deref, update goes through symref' '
	(
	cd repo &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	oid3=$(cat "$TRASH_DIRECTORY/oid3") &&
	grit update-ref refs/heads/sym-target2 "$oid2" &&
	git symbolic-ref refs/heads/sym-link2 refs/heads/sym-target2 &&
	grit update-ref refs/heads/sym-link2 "$oid3" &&
	grit rev-parse refs/heads/sym-target2 >actual &&
	echo "$oid3" >expect &&
	test_cmp expect actual
	)
'

# ── --stdin verify command ───────────────────────────────────────────────────

test_expect_success 'stdin: verify with correct value succeeds' '
	(
	cd repo &&
	oid4=$(cat "$TRASH_DIRECTORY/oid4") &&
	grit update-ref refs/heads/verify-test "$oid4" &&
	printf "verify refs/heads/verify-test %s\n" "$oid4" |
		grit update-ref --stdin
	)
'

test_expect_success 'stdin: verify with wrong value fails' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	printf "verify refs/heads/verify-test %s\n" "$oid1" |
		test_must_fail grit update-ref --stdin 2>err &&
	test -s err
	)
'

test_expect_success 'stdin: verify nonexistent ref with zero SHA succeeds' '
	(
	cd repo &&
	ZERO=$(printf "%040d" 0) &&
	printf "verify refs/heads/no-such-ref %s\n" "$ZERO" |
		grit update-ref --stdin
	)
'

test_expect_success 'stdin: verify nonexistent ref with non-zero SHA fails' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	printf "verify refs/heads/no-such-ref %s\n" "$oid1" |
		test_must_fail grit update-ref --stdin 2>err &&
	test -s err
	)
'

# ── stdin: create with old-value ─────────────────────────────────────────────

test_expect_success 'stdin: create new ref' '
	(
	cd repo &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	printf "create refs/heads/stdin-new %s\n" "$oid2" |
		grit update-ref --stdin &&
	grit rev-parse refs/heads/stdin-new >actual &&
	echo "$oid2" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'stdin: create fails if ref already exists' '
	(
	cd repo &&
	oid3=$(cat "$TRASH_DIRECTORY/oid3") &&
	printf "create refs/heads/stdin-new %s\n" "$oid3" |
		test_must_fail grit update-ref --stdin 2>err &&
	test -s err
	)
'

# ── stdin: delete command ────────────────────────────────────────────────────

test_expect_success 'stdin: delete ref' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	grit update-ref refs/heads/stdin-del "$oid1" &&
	printf "delete refs/heads/stdin-del %s\n" "$oid1" |
		grit update-ref --stdin &&
	test_must_fail grit rev-parse refs/heads/stdin-del 2>err
	)
'

test_expect_success 'stdin: delete with wrong old value fails' '
	(
	cd repo &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	oid4=$(cat "$TRASH_DIRECTORY/oid4") &&
	grit update-ref refs/heads/stdin-del2 "$oid2" &&
	printf "delete refs/heads/stdin-del2 %s\n" "$oid4" |
		test_must_fail grit update-ref --stdin 2>err &&
	test -s err
	)
'

# ── stdin: update command ────────────────────────────────────────────────────

test_expect_success 'stdin: update with old-value verification' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	oid3=$(cat "$TRASH_DIRECTORY/oid3") &&
	grit update-ref refs/heads/stdin-upd "$oid1" &&
	printf "update refs/heads/stdin-upd %s %s\n" "$oid3" "$oid1" |
		grit update-ref --stdin &&
	grit rev-parse refs/heads/stdin-upd >actual &&
	echo "$oid3" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'stdin: update with wrong old value fails atomically' '
	(
	cd repo &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	oid4=$(cat "$TRASH_DIRECTORY/oid4") &&
	printf "update refs/heads/stdin-upd %s %s\n" "$oid4" "$oid2" |
		test_must_fail grit update-ref --stdin 2>err &&
	test -s err &&
	oid3=$(cat "$TRASH_DIRECTORY/oid3") &&
	grit rev-parse refs/heads/stdin-upd >actual &&
	echo "$oid3" >expect &&
	test_cmp expect actual
	)
'

# ── stdin -z mode ────────────────────────────────────────────────────────────

test_expect_success 'stdin -z: create with NUL-separated commands' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	printf "create refs/heads/nul-ref %s" "$oid1" |
		grit update-ref -z --stdin &&
	grit rev-parse refs/heads/nul-ref >actual &&
	echo "$oid1" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'stdin -z: verify with NUL-separated commands' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	printf "verify refs/heads/nul-ref %s" "$oid1" |
		grit update-ref -z --stdin
	)
'

test_expect_success 'stdin -z: delete with NUL-separated commands' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	printf "delete refs/heads/nul-ref %s" "$oid1" |
		grit update-ref -z --stdin &&
	test_must_fail grit rev-parse refs/heads/nul-ref 2>err
	)
'

# ── edge cases ───────────────────────────────────────────────────────────────

test_expect_success 'update-ref with no arguments fails' '
	(
	cd repo &&
	test_must_fail grit update-ref 2>err
	)
'

test_expect_success 'update-ref with invalid SHA fails' '
	(
	cd repo &&
	test_must_fail grit update-ref refs/heads/bad "not-a-sha" 2>err &&
	test -s err
	)
'

test_expect_success 'update-ref can set HEAD-like refs' '
	(
	cd repo &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	grit update-ref refs/heads/head-test "$oid2" &&
	grit rev-parse refs/heads/head-test >actual &&
	echo "$oid2" >expect &&
	test_cmp expect actual
	)
'

test_done
