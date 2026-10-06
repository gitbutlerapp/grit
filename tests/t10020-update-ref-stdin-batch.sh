#!/bin/sh
# Tests for update-ref --stdin batch mode: create, update, delete, verify
# commands, error handling, and -z NUL-terminated mode.

test_description='update-ref --stdin batch operations'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup: repo with commits' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&

	echo "base" >file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "first" &&

	echo "second" >>file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "second" &&

	echo "third" >>file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "third" &&

	HEAD1=$(grit rev-parse HEAD) &&
	HEAD2=$(grit rev-parse HEAD~1) &&
	HEAD3=$(grit rev-parse HEAD~2) &&

	echo "$HEAD1" >"$TRASH_DIRECTORY/oid1" &&
	echo "$HEAD2" >"$TRASH_DIRECTORY/oid2" &&
	echo "$HEAD3" >"$TRASH_DIRECTORY/oid3"
	)
'

# ── create command ────────────────────────────────────────────────────────────

test_expect_success 'stdin: single create' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	printf "create refs/heads/batch-one %s\n" "$oid1" |
		grit update-ref --stdin &&
	grit rev-parse refs/heads/batch-one >actual &&
	echo "$oid1" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'stdin: multiple creates in one batch' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	oid3=$(cat "$TRASH_DIRECTORY/oid3") &&
	printf "create refs/batch/a %s\ncreate refs/batch/b %s\ncreate refs/batch/c %s\n" \
		"$oid1" "$oid2" "$oid3" |
		grit update-ref --stdin &&
	grit rev-parse refs/batch/a >actual_a &&
	grit rev-parse refs/batch/b >actual_b &&
	grit rev-parse refs/batch/c >actual_c &&
	echo "$oid1" >expect_a &&
	echo "$oid2" >expect_b &&
	echo "$oid3" >expect_c &&
	test_cmp expect_a actual_a &&
	test_cmp expect_b actual_b &&
	test_cmp expect_c actual_c
	)
'

test_expect_success 'stdin: create fails if ref already exists' '
	(
	cd repo &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	printf "create refs/heads/batch-one %s\n" "$oid2" |
		test_must_fail grit update-ref --stdin 2>err
	)
'

# ── update command ────────────────────────────────────────────────────────────

test_expect_success 'stdin: update with correct old value' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	printf "update refs/heads/batch-one %s %s\n" "$oid2" "$oid1" |
		grit update-ref --stdin &&
	grit rev-parse refs/heads/batch-one >actual &&
	echo "$oid2" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'stdin: update with wrong old value fails' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	oid3=$(cat "$TRASH_DIRECTORY/oid3") &&
	printf "update refs/heads/batch-one %s %s\n" "$oid3" "$oid1" |
		test_must_fail grit update-ref --stdin 2>err
	)
'

test_expect_success 'stdin: update without old value always succeeds' '
	(
	cd repo &&
	oid3=$(cat "$TRASH_DIRECTORY/oid3") &&
	printf "update refs/heads/batch-one %s\n" "$oid3" |
		grit update-ref --stdin &&
	grit rev-parse refs/heads/batch-one >actual &&
	echo "$oid3" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'stdin: update nonexistent ref creates it' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	printf "update refs/heads/new-from-update %s\n" "$oid1" |
		grit update-ref --stdin &&
	grit rev-parse refs/heads/new-from-update >actual &&
	echo "$oid1" >expect &&
	test_cmp expect actual
	)
'

# ── delete command ────────────────────────────────────────────────────────────

test_expect_success 'stdin: delete ref with correct old value' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	grit update-ref refs/heads/to-delete "$oid1" &&
	printf "delete refs/heads/to-delete %s\n" "$oid1" |
		grit update-ref --stdin &&
	test_must_fail grit show-ref --verify refs/heads/to-delete
	)
'

test_expect_success 'stdin: delete with wrong old value fails' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	grit update-ref refs/heads/to-delete2 "$oid1" &&
	printf "delete refs/heads/to-delete2 %s\n" "$oid2" |
		test_must_fail grit update-ref --stdin 2>err &&
	grit show-ref --verify refs/heads/to-delete2
	)
'

test_expect_success 'stdin: delete without old value' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	grit update-ref refs/heads/to-delete3 "$oid1" &&
	printf "delete refs/heads/to-delete3\n" |
		grit update-ref --stdin &&
	test_must_fail grit show-ref --verify refs/heads/to-delete3
	)
'

test_expect_success 'stdin: delete nonexistent ref without old value succeeds' '
	(
	cd repo &&
	printf "delete refs/heads/nonexistent\n" |
		grit update-ref --stdin
	)
'

# ── verify command ────────────────────────────────────────────────────────────

test_expect_success 'stdin: verify correct value succeeds' '
	(
	cd repo &&
	oid3=$(cat "$TRASH_DIRECTORY/oid3") &&
	printf "verify refs/heads/batch-one %s\n" "$oid3" |
		grit update-ref --stdin
	)
'

test_expect_success 'stdin: verify wrong value fails' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	printf "verify refs/heads/batch-one %s\n" "$oid1" |
		test_must_fail grit update-ref --stdin 2>err
	)
'

test_expect_success 'stdin: verify nonexistent ref with zero OID succeeds' '
	(
	cd repo &&
	printf "verify refs/heads/does-not-exist 0000000000000000000000000000000000000000\n" |
		grit update-ref --stdin
	)
'

test_expect_success 'stdin: verify nonexistent ref with non-zero OID fails' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	printf "verify refs/heads/does-not-exist %s\n" "$oid1" |
		test_must_fail grit update-ref --stdin 2>err
	)
'

# ── mixed commands ────────────────────────────────────────────────────────────

test_expect_success 'stdin: mixed create + update + delete in one batch' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	oid3=$(cat "$TRASH_DIRECTORY/oid3") &&
	grit update-ref refs/heads/mix-update "$oid1" &&
	grit update-ref refs/heads/mix-delete "$oid2" &&
	printf "create refs/heads/mix-new %s\nupdate refs/heads/mix-update %s %s\ndelete refs/heads/mix-delete %s\n" \
		"$oid3" "$oid2" "$oid1" "$oid2" |
		grit update-ref --stdin &&
	grit rev-parse refs/heads/mix-new >actual_new &&
	echo "$oid3" >expect_new &&
	test_cmp expect_new actual_new &&
	grit rev-parse refs/heads/mix-update >actual_upd &&
	echo "$oid2" >expect_upd &&
	test_cmp expect_upd actual_upd &&
	test_must_fail grit show-ref --verify refs/heads/mix-delete
	)
'

# ── empty and whitespace ─────────────────────────────────────────────────────

test_expect_success 'stdin: empty input succeeds' '
	(
	cd repo &&
	printf "" | grit update-ref --stdin
	)
'

test_expect_success 'stdin: blank lines are ignored' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	printf "\ncreate refs/heads/blank-test %s\n\n" "$oid1" |
		grit update-ref --stdin &&
	grit rev-parse refs/heads/blank-test >actual &&
	echo "$oid1" >expect &&
	test_cmp expect actual
	)
'

# ── invalid input ─────────────────────────────────────────────────────────────

test_expect_success 'stdin: unknown command fails' '
	(
	cd repo &&
	printf "frobnicate refs/heads/main\n" |
		test_must_fail grit update-ref --stdin 2>err
	)
'

test_expect_success 'stdin: create with missing OID fails' '
	(
	cd repo &&
	printf "create refs/heads/bad\n" |
		test_must_fail grit update-ref --stdin 2>err
	)
'

# ── -z NUL-terminated mode ───────────────────────────────────────────────────

test_expect_success 'stdin -z: single create' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	printf "create refs/heads/nul-one %s\0" "$oid1" |
		grit update-ref --stdin -z &&
	grit rev-parse refs/heads/nul-one >actual &&
	echo "$oid1" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'stdin -z: multiple commands' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	printf "create refs/nul/a %s\0create refs/nul/b %s\0" "$oid1" "$oid2" |
		grit update-ref --stdin -z &&
	grit rev-parse refs/nul/a >actual_a &&
	grit rev-parse refs/nul/b >actual_b &&
	echo "$oid1" >expect_a &&
	echo "$oid2" >expect_b &&
	test_cmp expect_a actual_a &&
	test_cmp expect_b actual_b
	)
'

test_expect_success 'stdin -z: update with old value check' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	printf "update refs/heads/nul-one %s %s\0" "$oid2" "$oid1" |
		grit update-ref --stdin -z &&
	grit rev-parse refs/heads/nul-one >actual &&
	echo "$oid2" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'stdin -z: delete' '
	(
	cd repo &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	printf "delete refs/heads/nul-one %s\0" "$oid2" |
		grit update-ref --stdin -z &&
	test_must_fail grit show-ref --verify refs/heads/nul-one
	)
'

test_expect_success 'stdin -z: verify' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	printf "verify refs/nul/a %s\0" "$oid1" |
		grit update-ref --stdin -z
	)
'

test_expect_success 'stdin -z: mixed create and delete' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	printf "create refs/nul/mix %s\0delete refs/nul/a %s\0" "$oid1" "$oid1" |
		grit update-ref --stdin -z &&
	grit rev-parse refs/nul/mix >actual &&
	echo "$oid1" >expect &&
	test_cmp expect actual &&
	test_must_fail grit show-ref --verify refs/nul/a
	)
'

# ── --no-deref ────────────────────────────────────────────────────────────────

test_expect_success '--no-deref updates symref target directly' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	grit update-ref refs/heads/noderef-target "$oid1" &&
	git symbolic-ref refs/heads/noderef-sym refs/heads/noderef-target &&
	grit update-ref --no-deref refs/heads/noderef-sym "$oid2" &&
	grit rev-parse refs/heads/noderef-sym >actual &&
	echo "$oid2" >expect &&
	test_cmp expect actual &&
	grit rev-parse refs/heads/noderef-target >actual_target &&
	echo "$oid1" >expect_target &&
	test_cmp expect_target actual_target
	)
'

# ── -m reflog message ─────────────────────────────────────────────────────────

test_expect_success '-m sets reflog message' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	grit update-ref -m "test message" refs/heads/reflog-test "$oid1" &&
	grit rev-parse refs/heads/reflog-test >actual &&
	echo "$oid1" >expect &&
	test_cmp expect actual
	)
'

# ── many refs in one batch ────────────────────────────────────────────────────

test_expect_success 'stdin: create 20 refs in one batch' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	i=0 &&
	while test $i -lt 20
	do
		printf "create refs/many/ref-%03d %s\n" "$i" "$oid1"
		i=$(($i + 1))
	done | grit update-ref --stdin &&
	grit for-each-ref --format="%(refname)" refs/many >actual &&
	test_line_count = 20 actual
	)
'

test_expect_success 'stdin: delete 20 refs in one batch' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	i=0 &&
	while test $i -lt 20
	do
		printf "delete refs/many/ref-%03d %s\n" "$i" "$oid1"
		i=$(($i + 1))
	done | grit update-ref --stdin &&
	grit for-each-ref --format="%(refname)" refs/many >actual &&
	test_must_be_empty actual
	)
'

# ── update-ref -d (non-stdin) ────────────────────────────────────────────────

test_expect_success 'update-ref -d deletes a ref' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	grit update-ref refs/heads/del-direct "$oid1" &&
	grit update-ref -d refs/heads/del-direct "$oid1" &&
	test_must_fail grit show-ref --verify refs/heads/del-direct
	)
'

test_expect_success 'update-ref -d with stale old value fails' '
	(
	cd repo &&
	oid1=$(cat "$TRASH_DIRECTORY/oid1") &&
	oid2=$(cat "$TRASH_DIRECTORY/oid2") &&
	grit update-ref refs/heads/del-stale "$oid1" &&
	test_must_fail grit update-ref -d refs/heads/del-stale "$oid2"
	)
'

test_done
