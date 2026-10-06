#!/bin/sh
# Tests for update-ref --stdin -z (NUL-terminated) and related options.

test_description='update-ref --stdin with NUL terminator'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- setup -----------------------------------------------------------------

test_expect_success 'setup: repo with initial commits' '
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
	$REAL_GIT commit -m "third"
	)
'

# -- basic update-ref (non-stdin) -------------------------------------------

test_expect_success 'update-ref creates a new ref' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD) &&
	grit update-ref refs/heads/newbranch "$sha" &&
	grit rev-parse refs/heads/newbranch >actual &&
	echo "$sha" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update-ref updates an existing ref' '
	(
	cd repo &&
	old=$(grit rev-parse HEAD) &&
	new=$(grit rev-parse HEAD~1) &&
	grit update-ref refs/heads/newbranch "$new" &&
	grit rev-parse refs/heads/newbranch >actual &&
	echo "$new" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update-ref with old value check succeeds' '
	(
	cd repo &&
	cur=$(grit rev-parse refs/heads/newbranch) &&
	new=$(grit rev-parse HEAD) &&
	grit update-ref refs/heads/newbranch "$new" "$cur" &&
	grit rev-parse refs/heads/newbranch >actual &&
	echo "$new" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update-ref with wrong old value fails' '
	(
	cd repo &&
	wrong=$(grit rev-parse HEAD~2) &&
	new=$(grit rev-parse HEAD~1) &&
	test_must_fail grit update-ref refs/heads/newbranch "$new" "$wrong" 2>err
	)
'

test_expect_success 'update-ref -d deletes a ref' '
	(
	cd repo &&
	grit update-ref refs/heads/todelete $(grit rev-parse HEAD) &&
	grit show-ref --verify refs/heads/todelete &&
	grit update-ref -d refs/heads/todelete &&
	test_must_fail grit show-ref --verify refs/heads/todelete 2>/dev/null
	)
'

test_expect_success 'update-ref --no-deref on symbolic ref' '
	(
	cd repo &&
	grit symbolic-ref refs/heads/symlink refs/heads/master &&
	sha=$(grit rev-parse HEAD~1) &&
	grit update-ref --no-deref refs/heads/symlink "$sha" &&
	# symlink should now be a regular ref, not symbolic
	grit rev-parse refs/heads/symlink >actual &&
	echo "$sha" >expect &&
	test_cmp expect actual
	)
'

# -- --stdin (newline-terminated) -------------------------------------------

test_expect_success 'update-ref --stdin: create command' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD) &&
	printf "create refs/heads/stdin-test %s\n" "$sha" |
	grit update-ref --stdin &&
	grit rev-parse refs/heads/stdin-test >actual &&
	echo "$sha" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update-ref --stdin: update command' '
	(
	cd repo &&
	old=$(grit rev-parse HEAD) &&
	new=$(grit rev-parse HEAD~1) &&
	printf "update refs/heads/stdin-test %s %s\n" "$new" "$old" |
	grit update-ref --stdin &&
	grit rev-parse refs/heads/stdin-test >actual &&
	echo "$new" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update-ref --stdin: delete command' '
	(
	cd repo &&
	sha=$(grit rev-parse refs/heads/stdin-test) &&
	printf "delete refs/heads/stdin-test %s\n" "$sha" |
	grit update-ref --stdin &&
	test_must_fail grit show-ref --verify refs/heads/stdin-test 2>/dev/null
	)
'

test_expect_success 'update-ref --stdin: multiple commands' '
	(
	cd repo &&
	sha1=$(grit rev-parse HEAD) &&
	sha2=$(grit rev-parse HEAD~1) &&
	printf "create refs/heads/multi-a %s\ncreate refs/heads/multi-b %s\n" "$sha1" "$sha2" |
	grit update-ref --stdin &&
	grit rev-parse refs/heads/multi-a >actual_a &&
	grit rev-parse refs/heads/multi-b >actual_b &&
	echo "$sha1" >expect_a &&
	echo "$sha2" >expect_b &&
	test_cmp expect_a actual_a &&
	test_cmp expect_b actual_b
	)
'

# -- --stdin -z (NUL-terminated) -------------------------------------------

test_expect_success 'update-ref --stdin -z: create command' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD) &&
	printf "create refs/heads/nul-test %s\0" "$sha" |
	grit update-ref --stdin -z &&
	grit rev-parse refs/heads/nul-test >actual &&
	echo "$sha" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update-ref --stdin -z: update command' '
	(
	cd repo &&
	old=$(grit rev-parse HEAD) &&
	new=$(grit rev-parse HEAD~1) &&
	printf "update refs/heads/nul-test %s %s\0" "$new" "$old" |
	grit update-ref --stdin -z &&
	grit rev-parse refs/heads/nul-test >actual &&
	echo "$new" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update-ref --stdin -z: delete command' '
	(
	cd repo &&
	sha=$(grit rev-parse refs/heads/nul-test) &&
	printf "delete refs/heads/nul-test %s\0" "$sha" |
	grit update-ref --stdin -z &&
	test_must_fail grit show-ref --verify refs/heads/nul-test 2>/dev/null
	)
'

test_expect_success 'update-ref --stdin -z: multiple creates' '
	(
	cd repo &&
	sha1=$(grit rev-parse HEAD) &&
	sha2=$(grit rev-parse HEAD~1) &&
	printf "create refs/heads/nul-a %s\0create refs/heads/nul-b %s\0" "$sha1" "$sha2" |
	grit update-ref --stdin -z &&
	grit rev-parse refs/heads/nul-a >actual_a &&
	grit rev-parse refs/heads/nul-b >actual_b &&
	echo "$sha1" >expect_a &&
	echo "$sha2" >expect_b &&
	test_cmp expect_a actual_a &&
	test_cmp expect_b actual_b
	)
'

test_expect_success 'update-ref --stdin -z: update with wrong old fails' '
	(
	cd repo &&
	wrong=$(grit rev-parse HEAD~2) &&
	new=$(grit rev-parse HEAD) &&
	printf "update refs/heads/nul-a %s %s\0" "$new" "$wrong" |
	test_must_fail grit update-ref --stdin -z 2>err
	)
'

test_expect_success 'update-ref --stdin -z: create and delete in one batch' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD) &&
	del_sha=$(grit rev-parse refs/heads/nul-b) &&
	printf "create refs/heads/nul-c %s\0delete refs/heads/nul-b %s\0" "$sha" "$del_sha" |
	grit update-ref --stdin -z &&
	grit show-ref --verify refs/heads/nul-c &&
	test_must_fail grit show-ref --verify refs/heads/nul-b 2>/dev/null
	)
'

# -- reflog message ---------------------------------------------------------

test_expect_success 'update-ref -m sets reflog message' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD) &&
	grit update-ref -m "test message" refs/heads/reflog-test "$sha" &&
	grit rev-parse refs/heads/reflog-test >actual &&
	echo "$sha" >expect &&
	test_cmp expect actual
	)
'

# -- verify ref was created correctly ----------------------------------------

test_expect_success 'update-ref result visible via show-ref' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD~1) &&
	grit update-ref refs/heads/visible-test "$sha" &&
	grit show-ref --verify refs/heads/visible-test >actual &&
	grep "$sha" actual
	)
'

test_expect_success 'update-ref to same value is idempotent' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD) &&
	grit update-ref refs/heads/idem "$sha" &&
	grit update-ref refs/heads/idem "$sha" &&
	grit rev-parse refs/heads/idem >actual &&
	echo "$sha" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update-ref can point ref to older commit' '
	(
	cd repo &&
	old=$(grit rev-parse HEAD~2) &&
	grit update-ref refs/heads/oldpoint "$old" &&
	grit rev-parse refs/heads/oldpoint >actual &&
	echo "$old" >expect &&
	test_cmp expect actual
	)
'

# -- stdin error handling ---------------------------------------------------

test_expect_success 'update-ref --stdin rejects unknown command' '
	(
	cd repo &&
	printf "bogus refs/heads/foo\n" |
	test_must_fail grit update-ref --stdin 2>err
	)
'

test_expect_success 'update-ref --stdin -z rejects malformed input' '
	(
	cd repo &&
	printf "create\0" |
	test_must_fail grit update-ref --stdin -z 2>err
	)
'

# -- additional coverage ----------------------------------------------------

test_expect_success 'update-ref --stdin -z: verify command succeeds for existing ref' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD) &&
	grit update-ref refs/heads/verify-test "$sha" &&
	printf "verify refs/heads/verify-test %s\0" "$sha" |
	grit update-ref --stdin -z
	)
'

test_expect_success 'update-ref --stdin -z: verify command fails for wrong value' '
	(
	cd repo &&
	wrong=$(grit rev-parse HEAD~2) &&
	printf "verify refs/heads/verify-test %s\0" "$wrong" |
	test_must_fail grit update-ref --stdin -z 2>err
	)
'

test_expect_success 'update-ref can create ref in nested namespace' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD) &&
	grit update-ref refs/custom/deep/ref "$sha" &&
	grit rev-parse refs/custom/deep/ref >actual &&
	echo "$sha" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'update-ref --stdin: verify succeeds' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD) &&
	grit update-ref refs/heads/vtest "$sha" &&
	printf "verify refs/heads/vtest %s\n" "$sha" |
	grit update-ref --stdin
	)
'

test_expect_success 'update-ref --stdin: verify fails for mismatch' '
	(
	cd repo &&
	wrong=$(grit rev-parse HEAD~1) &&
	printf "verify refs/heads/vtest %s\n" "$wrong" |
	test_must_fail grit update-ref --stdin 2>err
	)
'

test_expect_success 'update-ref -d with old-value check succeeds' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD) &&
	grit update-ref refs/heads/dtest "$sha" &&
	grit update-ref -d refs/heads/dtest "$sha" &&
	test_must_fail grit show-ref --verify refs/heads/dtest 2>/dev/null
	)
'

test_expect_success 'update-ref -d with wrong old-value fails' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD) &&
	grit update-ref refs/heads/dtest2 "$sha" &&
	wrong=$(grit rev-parse HEAD~1) &&
	test_must_fail grit update-ref -d refs/heads/dtest2 "$wrong" 2>err &&
	grit show-ref --verify refs/heads/dtest2
	)
'

test_expect_success 'update-ref --stdin -z: empty input is ok' '
	(
	cd repo &&
	printf "" | grit update-ref --stdin -z
	)
'

test_done
