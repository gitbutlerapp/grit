#!/bin/sh
# Tests for symbolic-ref: read, write, delete, --short, --no-recurse, -q.

test_description='symbolic-ref read, write, delete and options'

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

	grit update-ref refs/heads/main "$C3" &&
	grit update-ref refs/heads/develop "$C2" &&
	grit update-ref refs/heads/feature "$C1" &&

	echo "$C1" >"$TRASH_DIRECTORY/oid_C1" &&
	echo "$C2" >"$TRASH_DIRECTORY/oid_C2" &&
	echo "$C3" >"$TRASH_DIRECTORY/oid_C3"
	)
'

# ── Basic read ───────────────────────────────────────────────────────────────

test_expect_success 'read HEAD symbolic-ref' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/main &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'read HEAD after changing it' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/develop &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/develop" >expect &&
	test_cmp expect actual
	)
'

# ── Basic write ──────────────────────────────────────────────────────────────

test_expect_success 'write HEAD to a branch' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/feature &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'write HEAD back to main' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/main &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'create custom symbolic-ref' '
	(
	cd repo &&
	grit symbolic-ref refs/symref/test refs/heads/develop &&
	grit symbolic-ref refs/symref/test >actual &&
	echo "refs/heads/develop" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update custom symbolic-ref target' '
	(
	cd repo &&
	grit symbolic-ref refs/symref/test refs/heads/feature &&
	grit symbolic-ref refs/symref/test >actual &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual
	)
'

# ── --short ──────────────────────────────────────────────────────────────────

test_expect_success '--short strips refs/heads/ from HEAD target' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/main &&
	grit symbolic-ref --short HEAD >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--short strips refs/heads/ from develop' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/develop &&
	grit symbolic-ref --short HEAD >actual &&
	echo "develop" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--short strips refs/heads/ from feature' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/feature &&
	grit symbolic-ref --short HEAD >actual &&
	echo "feature" >expect &&
	test_cmp expect actual
	)
'

# ── --delete / -d ────────────────────────────────────────────────────────────

test_expect_success 'delete custom symbolic-ref' '
	(
	cd repo &&
	grit symbolic-ref refs/symref/delme refs/heads/main &&
	grit symbolic-ref refs/symref/delme >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual &&
	grit symbolic-ref -d refs/symref/delme &&
	test_must_fail grit symbolic-ref refs/symref/delme 2>err
	)
'

test_expect_success 'delete nonexistent symbolic-ref fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref -d refs/symref/nonexist 2>err
	)
'

test_expect_success 'delete regular (non-symbolic) ref fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref -d refs/heads/main 2>err
	)
'

# ── --quiet / -q ─────────────────────────────────────────────────────────────

test_expect_success '-q suppresses error on non-symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref -q refs/heads/main >actual 2>err &&
	test_must_be_empty actual ||
	# Some impls still fail but quietly
	test_must_fail grit symbolic-ref -q refs/heads/main 2>err &&
	test_must_be_empty err
	)
'

test_expect_success '-q on valid symbolic ref still shows target' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/main &&
	grit symbolic-ref -q HEAD >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

# ── Chained symbolic refs and --no-recurse ───────────────────────────────────

test_expect_success 'chained symbolic refs resolve transitively' '
	(
	cd repo &&
	grit symbolic-ref refs/symref/chain1 refs/heads/main &&
	grit symbolic-ref refs/symref/chain2 refs/symref/chain1 &&
	grit symbolic-ref refs/symref/chain2 >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--no-recurse stops at first level' '
	(
	cd repo &&
	grit symbolic-ref --no-recurse refs/symref/chain2 >actual &&
	echo "refs/symref/chain1" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--no-recurse on direct symbolic ref is same as default' '
	(
	cd repo &&
	grit symbolic-ref refs/symref/chain1 >default_out &&
	grit symbolic-ref --no-recurse refs/symref/chain1 >no_recurse_out &&
	test_cmp default_out no_recurse_out
	)
'

# ── Reflog message (-m) ─────────────────────────────────────────────────────

test_expect_success 'symbolic-ref -m sets message without error' '
	(
	cd repo &&
	grit symbolic-ref -m "switching to develop" HEAD refs/heads/develop &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/develop" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref -m with empty message is rejected' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref -m "" HEAD refs/heads/main 2>err
	)
'

# ── HEAD behavior ────────────────────────────────────────────────────────────

test_expect_success 'HEAD resolves to correct commit via rev-parse' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_C3") &&
	grit symbolic-ref HEAD refs/heads/main &&
	grit rev-parse HEAD >actual &&
	echo "$C3" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'switching HEAD changes what rev-parse returns' '
	(
	cd repo &&
	C2=$(cat "$TRASH_DIRECTORY/oid_C2") &&
	grit symbolic-ref HEAD refs/heads/develop &&
	grit rev-parse HEAD >actual &&
	echo "$C2" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'switching HEAD changes what rev-parse returns (feature)' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_C1") &&
	grit symbolic-ref HEAD refs/heads/feature &&
	grit rev-parse HEAD >actual &&
	echo "$C1" >expect &&
	test_cmp expect actual
	)
'

# ── Edge cases ───────────────────────────────────────────────────────────────

test_expect_success 'symbolic-ref to nonexistent target still creates symref' '
	(
	cd repo &&
	grit symbolic-ref refs/symref/dangling refs/heads/nonexistent &&
	grit symbolic-ref refs/symref/dangling >actual &&
	echo "refs/heads/nonexistent" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'reading non-ref name fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref not-a-ref 2>err
	)
'

test_expect_success 'symbolic-ref in fresh repo sets HEAD' '
	(
	grit init fresh &&
	cd fresh &&
	grit symbolic-ref HEAD refs/heads/trunk &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/trunk" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--short on HEAD pointing to trunk' '
	(
	cd fresh &&
	grit symbolic-ref --short HEAD >actual &&
	echo "trunk" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref with -m and --short combined' '
	(
	cd repo &&
	grit symbolic-ref -m "test switch" HEAD refs/heads/main &&
	grit symbolic-ref --short HEAD >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'overwrite symbolic ref repeatedly' '
	(
	cd repo &&
	grit symbolic-ref refs/symref/flip refs/heads/main &&
	grit symbolic-ref refs/symref/flip refs/heads/develop &&
	grit symbolic-ref refs/symref/flip refs/heads/feature &&
	grit symbolic-ref refs/symref/flip refs/heads/main &&
	grit symbolic-ref refs/symref/flip >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

test_done
