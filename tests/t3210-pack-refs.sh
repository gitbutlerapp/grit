#!/bin/sh
# Test show-ref and update-ref behaviour with packed-refs files.
# grit does not have pack-refs, so we use git pack-refs to create
# packed-refs and then verify grit reads/writes around them correctly.

test_description='show-ref and update-ref with packed-refs'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# We need real git for pack-refs since grit doesn't have it.
# The test harness puts a grit wrapper at .bin/git, so we must
# find the real git binary outside of PATH.
REAL_GIT=""
for p in /usr/bin/git /usr/local/bin/git; do
	if test -x "$p"; then
		REAL_GIT="$p"
		break
	fi
done
if test -z "$REAL_GIT"; then
	echo "SKIP: real git not found" >&2
	exit 0
fi

test_expect_success 'setup: create repo with several refs' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "initial" >file.txt &&
	grit add file.txt &&
	grit commit -m "initial" &&
	HEAD_OID=$(grit rev-list --max-count=1 HEAD) &&
	grit update-ref refs/heads/branch-a $HEAD_OID &&
	grit update-ref refs/heads/branch-b $HEAD_OID &&
	grit update-ref refs/heads/branch-c $HEAD_OID &&
	grit update-ref refs/tags/v1.0 $HEAD_OID &&
	grit update-ref refs/tags/v2.0 $HEAD_OID
	)
'

test_expect_success 'show-ref lists all loose refs' '
	(
	cd repo &&
	grit show-ref >actual &&
	test $(wc -l <actual) -ge 6
	)
'

test_expect_success 'show-ref --verify with loose ref works' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/branch-a >actual &&
	test $(wc -l <actual) = 1
	)
'

test_expect_success 'show-ref --exists with loose ref returns 0' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/branch-a
	)
'

test_expect_success 'show-ref --exists with nonexistent ref returns nonzero' '
	(
	cd repo &&
	test_must_fail grit show-ref --exists refs/heads/does-not-exist
	)
'

test_expect_success 'pack all refs with real git' '
	(
	cd repo &&
	"$REAL_GIT" pack-refs --all &&
	test -f .git/packed-refs
	)
'

test_expect_success 'show-ref reads packed-refs correctly' '
	(
	cd repo &&
	grit show-ref >actual &&
	grep refs/heads/branch-a actual &&
	grep refs/heads/branch-b actual &&
	grep refs/heads/branch-c actual &&
	grep refs/tags/v1.0 actual &&
	grep refs/tags/v2.0 actual
	)
'

test_expect_success 'show-ref --verify works with packed ref' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/branch-a >actual &&
	test $(wc -l <actual) = 1
	)
'

test_expect_success 'show-ref --exists works with packed ref' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/branch-a
	)
'

test_expect_success 'show-ref --hash works with packed ref' '
	(
	cd repo &&
	grit show-ref --hash refs/heads/branch-a >actual &&
	test $(wc -l <actual) = 1 &&
	HEAD_OID=$(grit rev-list --max-count=1 HEAD) &&
	echo "$HEAD_OID" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --tags filters to tags only from packed' '
	(
	cd repo &&
	grit show-ref --tags >actual &&
	grep refs/tags/v1.0 actual &&
	grep refs/tags/v2.0 actual &&
	! grep refs/heads actual
	)
'

test_expect_success 'show-ref --branches filters to branches only from packed' '
	(
	cd repo &&
	grit show-ref --branches >actual &&
	grep refs/heads/branch-a actual &&
	! grep refs/tags actual
	)
'

test_expect_success 'update-ref creates loose ref that shadows packed' '
	(
	cd repo &&
	echo "new content" >file2.txt &&
	grit add file2.txt &&
	grit commit -m "second" &&
	NEW_OID=$(grit rev-list --max-count=1 HEAD) &&
	grit update-ref refs/heads/branch-a $NEW_OID &&
	grit show-ref --hash refs/heads/branch-a >actual &&
	echo "$NEW_OID" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref shows updated ref (loose overrides packed)' '
	(
	cd repo &&
	NEW_OID=$(grit rev-list --max-count=1 HEAD) &&
	grit show-ref >actual &&
	grep "$NEW_OID.*refs/heads/branch-a" actual
	)
'

test_expect_success 'other packed refs remain intact after update-ref' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/branch-b >actual &&
	test $(wc -l <actual) = 1 &&
	grit show-ref --verify refs/heads/branch-c >actual &&
	test $(wc -l <actual) = 1
	)
'

test_expect_success 'update-ref with old-value check succeeds when matching' '
	(
	cd repo &&
	CURR_OID=$(grit show-ref --hash refs/heads/branch-b) &&
	NEW_OID=$(grit rev-list --max-count=1 HEAD) &&
	grit update-ref refs/heads/branch-b $NEW_OID $CURR_OID
	)
'

test_expect_success 'update-ref with wrong old-value check fails' '
	(
	cd repo &&
	test_must_fail grit update-ref refs/heads/branch-c HEAD 0000000000000000000000000000000000000001
	)
'

test_expect_success 'create new ref in packed-only repo area' '
	(
	cd repo &&
	HEAD_OID=$(grit rev-list --max-count=1 HEAD) &&
	grit update-ref refs/heads/new-branch $HEAD_OID &&
	grit show-ref --exists refs/heads/new-branch
	)
'

test_expect_success 'show-ref --head includes HEAD' '
	(
	cd repo &&
	grit show-ref --head >actual &&
	grep "HEAD" actual
	)
'

test_expect_success 'show-ref with pattern filters results' '
	(
	cd repo &&
	grit show-ref refs/tags/v1.0 refs/tags/v2.0 >actual &&
	grep refs/tags actual &&
	! grep refs/heads actual
	)
'

test_expect_success 'show-ref --quiet suppresses output' '
	(
	cd repo &&
	grit show-ref --quiet refs/heads/master >actual 2>&1 &&
	test_must_be_empty actual
	)
'

test_expect_success 'update-ref --stdin can create refs' '
	(
	cd repo &&
	HEAD_OID=$(grit rev-list --max-count=1 HEAD) &&
	printf "create refs/stdin-test/one %s\n" "$HEAD_OID" |
	grit update-ref --stdin &&
	grit show-ref --exists refs/stdin-test/one
	)
'

test_expect_success 'update-ref --stdin can update refs' '
	(
	cd repo &&
	OLD_OID=$(grit show-ref --hash refs/stdin-test/one) &&
	FIRST_OID=$(grit rev-list HEAD | tail -1) &&
	printf "update refs/stdin-test/one %s %s\n" "$FIRST_OID" "$OLD_OID" |
	grit update-ref --stdin &&
	grit show-ref --hash refs/stdin-test/one >actual &&
	echo "$FIRST_OID" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update-ref --stdin can delete refs' '
	(
	cd repo &&
	OLD_OID=$(grit show-ref --hash refs/stdin-test/one) &&
	printf "delete refs/stdin-test/one %s\n" "$OLD_OID" |
	grit update-ref --stdin &&
	test_must_fail grit show-ref --exists refs/stdin-test/one
	)
'

test_expect_success 'update-ref -d deletes loose ref' '
	(
	cd repo &&
	HEAD_OID=$(grit rev-list --max-count=1 HEAD) &&
	grit update-ref refs/to-delete $HEAD_OID &&
	grit show-ref --exists refs/to-delete &&
	grit update-ref -d refs/to-delete &&
	test_must_fail grit show-ref --exists refs/to-delete
	)
'

test_expect_success 'show-ref --abbrev abbreviates OIDs' '
	(
	cd repo &&
	grit show-ref --abbrev >actual &&
	# abbreviated OIDs should be shorter than 40 chars
	first_oid=$(head -1 actual | awk "{print \$1}") &&
	test ${#first_oid} -lt 40
	)
'

test_expect_success 'show-ref -s shows only OIDs' '
	(
	cd repo &&
	grit show-ref -s >actual &&
	# each line should be a bare OID (40 hex chars)
	while read line; do
		test ${#line} = 40 || exit 1
	done <actual
	)
'

test_expect_success 'update-ref --no-deref on symbolic ref' '
	(
	cd repo &&
	HEAD_OID=$(grit rev-list --max-count=1 HEAD) &&
	grit update-ref --no-deref refs/heads/noderef-test $HEAD_OID &&
	grit show-ref --exists refs/heads/noderef-test
	)
'

test_expect_success 'repack does not break packed ref reading' '
	(
	cd repo &&
	"$REAL_GIT" pack-refs --all &&
	grit show-ref >after_repack &&
	grep refs/heads/master after_repack &&
	grep refs/heads/new-branch after_repack
	)
'

test_done
