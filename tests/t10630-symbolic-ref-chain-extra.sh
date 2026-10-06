#!/bin/sh
# Tests for grit symbolic-ref with chains, --short, --no-recurse, -d, -q.

test_description='grit symbolic-ref chain and extra options'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with branches' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "initial" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial commit" &&
	"$REAL_GIT" branch feature &&
	"$REAL_GIT" branch release
	)
'

###########################################################################
# Section 2: Basic symbolic-ref read
###########################################################################

test_expect_success 'symbolic-ref HEAD reads current branch' '
	(
	cd repo &&
	grit symbolic-ref HEAD >actual &&
	"$REAL_GIT" symbolic-ref HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref HEAD shows refs/heads/main' '
	(
	cd repo &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref HEAD after checkout' '
	(
	cd repo &&
	"$REAL_GIT" checkout feature &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual &&
	"$REAL_GIT" checkout main
	)
'

###########################################################################
# Section 3: symbolic-ref write
###########################################################################

test_expect_success 'symbolic-ref can set HEAD' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/release &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/release" >expect &&
	test_cmp expect actual &&
	grit symbolic-ref HEAD refs/heads/main
	)
'

test_expect_success 'symbolic-ref can create custom symref' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/alias refs/heads/feature &&
	grit symbolic-ref refs/heads/alias >actual &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'custom symref resolves to same OID as target' '
	(
	cd repo &&
	ALIAS_OID=$(grit rev-parse refs/heads/alias) &&
	TARGET_OID=$(grit rev-parse refs/heads/feature) &&
	test "$ALIAS_OID" = "$TARGET_OID"
	)
'

###########################################################################
# Section 4: --short
###########################################################################

test_expect_success 'symbolic-ref --short HEAD' '
	(
	cd repo &&
	grit symbolic-ref --short HEAD >actual &&
	"$REAL_GIT" symbolic-ref --short HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref --short strips refs/heads/' '
	(
	cd repo &&
	grit symbolic-ref --short HEAD >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref --short on custom symref' '
	(
	cd repo &&
	grit symbolic-ref --short refs/heads/alias >actual &&
	echo "feature" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 5: --delete (-d)
###########################################################################

test_expect_success 'symbolic-ref -d deletes a symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/to-delete refs/heads/main &&
	grit symbolic-ref refs/heads/to-delete >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual &&
	grit symbolic-ref -d refs/heads/to-delete &&
	test_must_fail grit symbolic-ref refs/heads/to-delete 2>/dev/null
	)
'

test_expect_success 'symbolic-ref --delete also works' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/to-delete2 refs/heads/feature &&
	grit symbolic-ref --delete refs/heads/to-delete2 &&
	test_must_fail grit symbolic-ref refs/heads/to-delete2 2>/dev/null
	)
'

test_expect_success 'symbolic-ref -d on non-symbolic ref fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref -d refs/heads/main 2>/dev/null
	)
'

###########################################################################
# Section 6: --quiet (-q)
###########################################################################

test_expect_success 'symbolic-ref -q on symbolic ref succeeds quietly' '
	(
	cd repo &&
	grit symbolic-ref -q HEAD >actual &&
	test -s actual
	)
'

test_expect_success 'symbolic-ref -q on non-symbolic ref fails quietly' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref -q refs/heads/main 2>err &&
	test_must_be_empty err
	)
'

###########################################################################
# Section 7: Chained symbolic refs
###########################################################################

test_expect_success 'create chain: sym-a -> sym-b -> feature' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/sym-b refs/heads/feature &&
	grit symbolic-ref refs/heads/sym-a refs/heads/sym-b
	)
'

test_expect_success 'reading sym-a resolves through chain to feature' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/sym-a >actual &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-parse resolves through symref chain' '
	(
	cd repo &&
	CHAIN_OID=$(grit rev-parse refs/heads/sym-a) &&
	FEATURE_OID=$(grit rev-parse refs/heads/feature) &&
	test "$CHAIN_OID" = "$FEATURE_OID"
	)
'

###########################################################################
# Section 8: --no-recurse
###########################################################################

test_expect_success 'symbolic-ref --no-recurse stops after one dereference' '
	(
	cd repo &&
	grit symbolic-ref --no-recurse refs/heads/sym-a >actual &&
	echo "refs/heads/sym-b" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref without --no-recurse resolves fully' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/sym-a >actual &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref --no-recurse on single-level symref' '
	(
	cd repo &&
	grit symbolic-ref --no-recurse refs/heads/sym-b >actual &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref --no-recurse on HEAD' '
	(
	cd repo &&
	grit symbolic-ref --no-recurse HEAD >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 9: -m (reflog message)
###########################################################################

test_expect_success 'symbolic-ref -m sets reflog message' '
	(
	cd repo &&
	grit symbolic-ref -m "switching to release" HEAD refs/heads/release &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/release" >expect &&
	test_cmp expect actual &&
	grit symbolic-ref HEAD refs/heads/main
	)
'

###########################################################################
# Section 10: Edge cases
###########################################################################

test_expect_success 'symbolic-ref with no args on non-symref fails' '
	(
	cd repo &&
	test_must_fail grit symbolic-ref refs/heads/feature 2>/dev/null
	)
'

test_expect_success 'symbolic-ref to nonexistent target is allowed' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/dangling refs/heads/nonexistent &&
	grit symbolic-ref refs/heads/dangling >actual &&
	echo "refs/heads/nonexistent" >expect &&
	test_cmp expect actual &&
	grit symbolic-ref -d refs/heads/dangling
	)
'

test_expect_success 'symbolic-ref cleanup: delete chain refs' '
	(
	cd repo &&
	grit symbolic-ref -d refs/heads/sym-a &&
	grit symbolic-ref -d refs/heads/sym-b &&
	grit symbolic-ref -d refs/heads/alias &&
	test_must_fail grit symbolic-ref refs/heads/sym-a 2>/dev/null &&
	test_must_fail grit symbolic-ref refs/heads/sym-b 2>/dev/null
	)
'

test_expect_success 'symbolic-ref --short on HEAD after operations' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/main &&
	grit symbolic-ref --short HEAD >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref matches git for HEAD' '
	(
	cd repo &&
	grit symbolic-ref HEAD >actual &&
	"$REAL_GIT" symbolic-ref HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref --short matches git for HEAD' '
	(
	cd repo &&
	grit symbolic-ref --short HEAD >actual &&
	"$REAL_GIT" symbolic-ref --short HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref can point HEAD to feature' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/feature &&
	grit symbolic-ref HEAD >actual &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'symbolic-ref can point HEAD back to main' '
	(
	cd repo &&
	grit symbolic-ref HEAD refs/heads/main &&
	grit symbolic-ref --short HEAD >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'create triple chain: c -> b -> a -> main' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/chain-a refs/heads/main &&
	grit symbolic-ref refs/heads/chain-b refs/heads/chain-a &&
	grit symbolic-ref refs/heads/chain-c refs/heads/chain-b &&
	grit symbolic-ref refs/heads/chain-c >actual &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'triple chain --no-recurse shows immediate target' '
	(
	cd repo &&
	grit symbolic-ref --no-recurse refs/heads/chain-c >actual &&
	echo "refs/heads/chain-b" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'triple chain rev-parse resolves to main OID' '
	(
	cd repo &&
	CHAIN_OID=$(grit rev-parse refs/heads/chain-c) &&
	MASTER_OID=$(grit rev-parse refs/heads/main) &&
	test "$CHAIN_OID" = "$MASTER_OID"
	)
'

test_expect_success 'cleanup triple chain' '
	(
	cd repo &&
	grit symbolic-ref -d refs/heads/chain-c &&
	grit symbolic-ref -d refs/heads/chain-b &&
	grit symbolic-ref -d refs/heads/chain-a
	)
'

test_done
