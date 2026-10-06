#!/bin/sh
# Tests for grit update-ref --stdin -z (NUL-terminated input).

test_description='grit update-ref --stdin with NUL line terminator'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with initial commit' '
	cd "$TRASH_DIRECTORY" &&
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "initial" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "first commit" &&
	echo "second" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit"
'

###########################################################################
# Section 2: Basic update-ref (non-stdin)
# Each block begins with `cd "$TRASH_DIRECTORY"` because this harness keeps
# the shell cwd across tests (unlike upstream git/t), so `cd repo` must start
# from the trash root every time.
###########################################################################

test_expect_success 'update-ref creates a new ref' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	grit update-ref refs/heads/new-branch "$HEAD_OID" &&
	grit show-ref --verify refs/heads/new-branch >actual &&
	echo "$HEAD_OID refs/heads/new-branch" >expect &&
	test_cmp expect actual
'

test_expect_success 'update-ref can update existing ref' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	OLD_OID=$("$REAL_GIT" rev-parse HEAD~1) &&
	grit update-ref refs/heads/new-branch "$OLD_OID" &&
	grit show-ref --verify refs/heads/new-branch >actual &&
	echo "$OLD_OID refs/heads/new-branch" >expect &&
	test_cmp expect actual
'

test_expect_success 'update-ref with old value check succeeds when matching' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	OLD_OID=$("$REAL_GIT" rev-parse HEAD~1) &&
	NEW_OID=$("$REAL_GIT" rev-parse HEAD) &&
	grit update-ref refs/heads/new-branch "$NEW_OID" "$OLD_OID"
'

test_expect_success 'update-ref with wrong old value fails' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	WRONG_OID=$("$REAL_GIT" rev-parse HEAD~1) &&
	test_must_fail grit update-ref refs/heads/new-branch "$WRONG_OID" "$WRONG_OID"
'

test_expect_success 'update-ref -d deletes a ref' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	grit update-ref refs/heads/to-delete "$HEAD_OID" &&
	grit show-ref --exists refs/heads/to-delete &&
	grit update-ref -d refs/heads/to-delete &&
	test_must_fail grit show-ref --exists refs/heads/to-delete
'

###########################################################################
# Section 3: --stdin with newline-terminated input
###########################################################################

test_expect_success 'update-ref --stdin create command' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	printf "create refs/heads/stdin-branch %s\n" "$HEAD_OID" |
		grit update-ref --stdin &&
	grit show-ref --verify refs/heads/stdin-branch >actual &&
	echo "$HEAD_OID refs/heads/stdin-branch" >expect &&
	test_cmp expect actual
'

test_expect_success 'update-ref --stdin update command' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	OLD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	NEW_OID=$("$REAL_GIT" rev-parse HEAD~1) &&
	printf "update refs/heads/stdin-branch %s %s\n" "$NEW_OID" "$OLD_OID" |
		grit update-ref --stdin &&
	grit show-ref --verify refs/heads/stdin-branch >actual &&
	echo "$NEW_OID refs/heads/stdin-branch" >expect &&
	test_cmp expect actual
'

test_expect_success 'update-ref --stdin delete command' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	CURRENT=$("$REAL_GIT" rev-parse HEAD~1) &&
	printf "delete refs/heads/stdin-branch %s\n" "$CURRENT" |
		grit update-ref --stdin &&
	test_must_fail grit show-ref --exists refs/heads/stdin-branch
'

test_expect_success 'update-ref --stdin multiple commands in one batch' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	OLD_OID=$("$REAL_GIT" rev-parse HEAD~1) &&
	printf "create refs/heads/batch-a %s\ncreate refs/heads/batch-b %s\n" "$HEAD_OID" "$OLD_OID" |
		grit update-ref --stdin &&
	grit show-ref --verify refs/heads/batch-a &&
	grit show-ref --verify refs/heads/batch-b
'

###########################################################################
# Section 4: --stdin -z (NUL as line terminator)
###########################################################################

test_expect_success 'update-ref --stdin -z create command' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	printf "create refs/heads/nul-branch %s\0" "$HEAD_OID" |
		grit update-ref --stdin -z &&
	grit show-ref --verify refs/heads/nul-branch >actual &&
	echo "$HEAD_OID refs/heads/nul-branch" >expect &&
	test_cmp expect actual
'

test_expect_success 'update-ref --stdin -z update command' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	OLD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	NEW_OID=$("$REAL_GIT" rev-parse HEAD~1) &&
	printf "update refs/heads/nul-branch %s %s\0" "$NEW_OID" "$OLD_OID" |
		grit update-ref --stdin -z &&
	grit show-ref --verify refs/heads/nul-branch >actual &&
	echo "$NEW_OID refs/heads/nul-branch" >expect &&
	test_cmp expect actual
'

test_expect_success 'update-ref --stdin -z delete command' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	CURRENT=$("$REAL_GIT" rev-parse HEAD~1) &&
	printf "delete refs/heads/nul-branch %s\0" "$CURRENT" |
		grit update-ref --stdin -z &&
	test_must_fail grit show-ref --exists refs/heads/nul-branch
'

test_expect_success 'update-ref --stdin -z multiple creates' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	OLD_OID=$("$REAL_GIT" rev-parse HEAD~1) &&
	printf "create refs/heads/nul-a %s\0create refs/heads/nul-b %s\0" "$HEAD_OID" "$OLD_OID" |
		grit update-ref --stdin -z &&
	grit show-ref --verify refs/heads/nul-a &&
	grit show-ref --verify refs/heads/nul-b
'

test_expect_success 'update-ref --stdin -z create and delete in one batch' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	NUL_A_OID=$("$REAL_GIT" rev-parse refs/heads/nul-a) &&
	printf "delete refs/heads/nul-a %s\0create refs/heads/nul-c %s\0" "$NUL_A_OID" "$HEAD_OID" |
		grit update-ref --stdin -z &&
	test_must_fail grit show-ref --exists refs/heads/nul-a &&
	grit show-ref --verify refs/heads/nul-c
'

test_expect_success 'update-ref --stdin -z verify command succeeds on match' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	NUL_C_OID=$("$REAL_GIT" rev-parse refs/heads/nul-c) &&
	printf "verify refs/heads/nul-c %s\0" "$NUL_C_OID" |
		grit update-ref --stdin -z
'

test_expect_success 'update-ref --stdin -z verify command fails on mismatch' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	WRONG=$("$REAL_GIT" rev-parse HEAD~1) &&
	printf "verify refs/heads/nul-c %s\0" "$WRONG" |
		test_must_fail grit update-ref --stdin -z
'

###########################################################################
# Section 5: --no-deref
###########################################################################

test_expect_success 'update-ref --no-deref on symbolic ref updates symref itself' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	"$REAL_GIT" symbolic-ref refs/heads/symlink refs/heads/master &&
	grit update-ref --no-deref refs/heads/symlink "$HEAD_OID" &&
	test_must_fail "$REAL_GIT" symbolic-ref refs/heads/symlink 2>/dev/null &&
	STORED=$("$REAL_GIT" rev-parse refs/heads/symlink) &&
	test "$STORED" = "$HEAD_OID"
'

###########################################################################
# Section 6: Ref creation in subdirectories
###########################################################################

test_expect_success 'update-ref creates deeply nested ref' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	grit update-ref refs/custom/deep/nested/ref "$HEAD_OID" &&
	grit show-ref --verify refs/custom/deep/nested/ref >actual &&
	echo "$HEAD_OID refs/custom/deep/nested/ref" >expect &&
	test_cmp expect actual
'

test_expect_success 'update-ref creates refs/notes/ namespace' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	grit update-ref refs/notes/commits "$HEAD_OID" &&
	grit show-ref --verify refs/notes/commits
'

test_expect_success 'update-ref creates refs/stash ref' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	grit update-ref refs/stash "$HEAD_OID" &&
	grit show-ref --verify refs/stash
'

###########################################################################
# Section 7: update-ref -m (reflog message)
###########################################################################

test_expect_success 'update-ref -m sets reflog message' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	grit update-ref -m "test message" refs/heads/reflog-test "$HEAD_OID" &&
	grit show-ref --verify refs/heads/reflog-test
'

###########################################################################
# Section 8: Edge cases
###########################################################################

test_expect_success 'update-ref -d on nonexistent ref is silent' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	grit update-ref -d refs/heads/no-such-ref &&
	test_must_fail grit show-ref --exists refs/heads/no-such-ref
'

test_expect_success 'update-ref --stdin -z with empty input succeeds' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	printf "" | grit update-ref --stdin -z
'

test_expect_success 'update-ref --stdin with empty input succeeds' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	printf "" | grit update-ref --stdin
'

test_expect_success 'update-ref with invalid OID fails' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	test_must_fail grit update-ref refs/heads/bad-oid "not-a-valid-oid"
'

test_expect_success 'update-ref --stdin -z create then verify in single batch' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	printf "create refs/heads/batch-verify %s\0verify refs/heads/batch-verify %s\0" "$HEAD_OID" "$HEAD_OID" |
		grit update-ref --stdin -z &&
	grit show-ref --verify refs/heads/batch-verify
'

test_expect_success 'update-ref --stdin -z atomic: all-or-nothing on conflict' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	OLD_OID=$("$REAL_GIT" rev-parse HEAD~1) &&
	grit update-ref refs/heads/atomic-test "$HEAD_OID" &&
	printf "update refs/heads/atomic-test %s %s\0" "$OLD_OID" "$OLD_OID" |
		test_must_fail grit update-ref --stdin -z &&
	STORED=$("$REAL_GIT" rev-parse refs/heads/atomic-test) &&
	test "$STORED" = "$HEAD_OID"
'

test_expect_success 'update-ref --stdin -z handles update without old value' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	printf "update refs/heads/no-old-val %s\0" "$HEAD_OID" |
		grit update-ref --stdin -z &&
	grit show-ref --verify refs/heads/no-old-val
'

test_expect_success 'update-ref --stdin newline: create + update + delete batch' '
	cd "$TRASH_DIRECTORY" &&
	cd repo &&
	HEAD_OID=$("$REAL_GIT" rev-parse HEAD) &&
	OLD_OID=$("$REAL_GIT" rev-parse HEAD~1) &&
	printf "create refs/heads/combo-a %s\ncreate refs/heads/combo-b %s\n" "$HEAD_OID" "$OLD_OID" |
		grit update-ref --stdin &&
	COMBO_B=$("$REAL_GIT" rev-parse refs/heads/combo-b) &&
	printf "update refs/heads/combo-b %s %s\ndelete refs/heads/combo-a %s\n" "$HEAD_OID" "$COMBO_B" "$HEAD_OID" |
		grit update-ref --stdin &&
	test_must_fail grit show-ref --exists refs/heads/combo-a &&
	grit show-ref --verify refs/heads/combo-b
'

test_done
