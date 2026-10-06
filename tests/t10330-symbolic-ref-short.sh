#!/bin/sh
# Tests for symbolic-ref --short, --no-recurse, -d (delete), -q (quiet),
# creation, reading, and error handling.

test_description='symbolic-ref --short, --no-recurse, delete, quiet'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&

	EMPTY_TREE=$(printf "" | grit hash-object -w -t tree --stdin) &&

	A=$(grit commit-tree "$EMPTY_TREE" -m "initial") &&
	B=$(grit commit-tree "$EMPTY_TREE" -p "$A" -m "second") &&

	grit update-ref refs/heads/main "$B" &&
	grit update-ref refs/heads/develop "$A" &&
	grit update-ref refs/heads/feature "$A" &&

	git symbolic-ref HEAD refs/heads/main &&

	echo "$A" >"$TRASH_DIRECTORY/oid_A" &&
	echo "$B" >"$TRASH_DIRECTORY/oid_B"
	)
'

# ── read symbolic ref ────────────────────────────────────────────────────────

test_expect_success 'read HEAD as symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'read HEAD --short' '
	(
	cd repo &&
	grit symbolic-ref --short HEAD >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref shows full refname by default' '
	(
	cd repo &&
	grit symbolic-ref HEAD >actual &&
	grep "^refs/heads/" actual
	)
'

# ── create symbolic ref ──────────────────────────────────────────────────────

test_expect_success 'create a new symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/alias refs/heads/develop &&
	grit symbolic-ref refs/heads/alias >actual &&
	echo "refs/heads/develop" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic ref resolves to target value' '
	(
	cd repo &&
	A=$(cat "$TRASH_DIRECTORY/oid_A") &&
	grit rev-parse refs/heads/alias >actual &&
	echo "$A" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update symbolic ref to new target' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/alias refs/heads/main &&
	grit symbolic-ref refs/heads/alias >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'set HEAD to different branch' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/develop &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/develop" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'restore HEAD to main' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/main &&
	grit symbolic-ref --short HEAD >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

# ── --short ──────────────────────────────────────────────────────────────────

test_expect_success '--short strips refs/heads/ prefix' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/feature &&
	grit symbolic-ref --short HEAD >actual &&
	echo "feature" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--short on non-heads ref keeps more of path' '
	(
	cd repo &&
	grit symbolic-ref refs/symtest refs/tags/sometag 2>/dev/null;
	grit symbolic-ref HEAD refs/heads/main &&
	grit symbolic-ref --short HEAD >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--short with alias symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/alias refs/heads/develop &&
	grit symbolic-ref --short refs/heads/alias >actual &&
	echo "develop" >expect &&
	test_cmp expect actual
	)
'

# ── --no-recurse ─────────────────────────────────────────────────────────────

test_expect_success 'chain of symbolic refs: setup' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/chain1 refs/heads/main &&
	grit symbolic-ref refs/heads/chain2 refs/heads/chain1
	)
'

test_expect_success '--no-recurse stops after one level' '
	(
	cd repo &&
	grit symbolic-ref --no-recurse refs/heads/chain2 >actual &&
	echo "refs/heads/chain1" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'without --no-recurse, fully resolves chain' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/chain2 >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--no-recurse on direct symbolic ref same as regular' '
	(
	cd repo &&
	grit symbolic-ref --no-recurse refs/heads/chain1 >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

# ── --quiet (-q) ─────────────────────────────────────────────────────────────

test_expect_success '--quiet suppresses error for non-symbolic ref' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref --quiet refs/heads/main 2>err &&
	test_must_be_empty err
	)
'

test_expect_success '-q is alias for --quiet' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref -q refs/heads/main 2>err &&
	test_must_be_empty err
	)
'

test_expect_success 'without --quiet, non-symbolic ref shows error' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref refs/heads/main 2>err &&
	test -s err
	)
'

test_expect_success '--quiet still works for symbolic refs' '
	(
	cd repo &&
	grit symbolic-ref --quiet HEAD >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

# ── --delete (-d) ────────────────────────────────────────────────────────────

test_expect_success 'delete a symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/to-delete refs/heads/main &&
	grit symbolic-ref -d refs/heads/to-delete &&
	test_must_fail grit symbolic-ref refs/heads/to-delete 2>err
	)
'

test_expect_success 'delete non-symbolic ref fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref -d refs/heads/main 2>err
	)
'

test_expect_success 'delete nonexistent ref fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref -d refs/heads/nonexistent 2>err
	)
'

test_expect_success 'after delete, ref no longer resolves' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/del2 refs/heads/develop &&
	grit symbolic-ref -d refs/heads/del2 &&
	test_must_fail grit rev-parse refs/heads/del2 2>err
	)
'

# ── -m (reflog message) ─────────────────────────────────────────────────────

test_expect_success '-m sets reflog message on symbolic ref update' '
	(
	cd repo &&
	grit symbolic-ref -m "switching branch" HEAD refs/heads/develop &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/develop" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '-m with create' '
	(
	cd repo &&
	grit symbolic-ref -m "create alias" refs/heads/msg-alias refs/heads/feature &&
	grit symbolic-ref refs/heads/msg-alias >actual &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual
	)
'

# ── error cases ──────────────────────────────────────────────────────────────

test_expect_success 'symbolic-ref with no args fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref 2>err
	)
'

test_expect_success 'reading non-symbolic ref fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref refs/heads/main 2>err &&
	test -s err
	)
'

test_expect_success 'symbolic-ref --short with --no-recurse' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/combo refs/heads/main &&
	grit symbolic-ref refs/heads/combo2 refs/heads/combo &&
	grit symbolic-ref --short --no-recurse refs/heads/combo2 >actual &&
	echo "combo" >expect &&
	test_cmp expect actual
	)
'

# ── restore HEAD ─────────────────────────────────────────────────────────────

test_expect_success 'restore HEAD to main for cleanup' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/main &&
	grit symbolic-ref --short HEAD >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

test_done
