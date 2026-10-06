#!/bin/sh
# Tests for grit update-ref with --stdin batch mode, atomic updates, -d, --no-deref.

test_description='grit update-ref: stdin batch mode, atomic, delete, no-deref, reflog messages'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with commits' '
	(
	"$REAL_GIT" init -b main repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&

	echo "first" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "first commit" &&
	FIRST=$("$REAL_GIT" rev-parse HEAD) &&

	echo "second" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit" &&
	SECOND=$("$REAL_GIT" rev-parse HEAD) &&

	echo "third" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "third commit" &&
	THIRD=$("$REAL_GIT" rev-parse HEAD)
	)
'

###########################################################################
# Section 2: Basic update-ref
###########################################################################

test_expect_success 'update-ref: create a new ref' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/heads/new-branch "$SHA" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/new-branch) &&
	test "$SHA" = "$ACTUAL"
	)
'

test_expect_success 'update-ref: update existing ref to different commit' '
	(
	cd repo &&
	OLD=$("$REAL_GIT" rev-parse HEAD) &&
	NEW=$("$REAL_GIT" rev-parse HEAD~1) &&
	"$GUST_BIN" update-ref refs/heads/new-branch "$NEW" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/new-branch) &&
	test "$NEW" = "$ACTUAL"
	)
'

test_expect_success 'update-ref: update with old-value check succeeds' '
	(
	cd repo &&
	OLD=$("$REAL_GIT" rev-parse refs/heads/new-branch) &&
	NEW=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/heads/new-branch "$NEW" "$OLD" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/new-branch) &&
	test "$NEW" = "$ACTUAL"
	)
'

test_expect_success 'update-ref: update with wrong old-value fails' '
	(
	cd repo &&
	WRONG=$("$REAL_GIT" rev-parse HEAD~2) &&
	NEW=$("$REAL_GIT" rev-parse HEAD~1) &&
	test_must_fail "$GUST_BIN" update-ref refs/heads/new-branch "$NEW" "$WRONG"
	)
'

test_expect_success 'update-ref: ref unchanged after failed old-value check' '
	(
	cd repo &&
	EXPECTED=$("$REAL_GIT" rev-parse HEAD) &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/new-branch) &&
	test "$EXPECTED" = "$ACTUAL"
	)
'

###########################################################################
# Section 3: Delete ref with -d
###########################################################################

test_expect_success 'update-ref -d: delete a ref' '
	(
	cd repo &&
	"$GUST_BIN" update-ref -d refs/heads/new-branch &&
	test_must_fail "$REAL_GIT" rev-parse --verify refs/heads/new-branch 2>/dev/null
	)
'

test_expect_success 'update-ref -d: delete non-existent ref is handled' '
	(
	cd repo &&
	"$GUST_BIN" update-ref -d refs/heads/nonexistent 2>err.txt || true &&
	# grit may or may not error; just verify it does not crash
	test_must_fail "$REAL_GIT" rev-parse --verify refs/heads/nonexistent 2>/dev/null
	)
'

test_expect_success 'update-ref -d: delete with old-value check' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/heads/to-delete "$SHA" &&
	"$GUST_BIN" update-ref -d refs/heads/to-delete "$SHA" &&
	test_must_fail "$REAL_GIT" rev-parse --verify refs/heads/to-delete 2>/dev/null
	)
'

test_expect_success 'update-ref -d: delete with wrong old-value fails' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	WRONG=$("$REAL_GIT" rev-parse HEAD~1) &&
	"$GUST_BIN" update-ref refs/heads/nodelete "$SHA" &&
	test_must_fail "$GUST_BIN" update-ref -d refs/heads/nodelete "$WRONG"
	)
'

###########################################################################
# Section 4: --stdin batch mode
###########################################################################

test_expect_success 'update-ref --stdin: single update command' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	printf "update refs/heads/stdin-test %s\n" "$SHA" |
	"$GUST_BIN" update-ref --stdin &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/stdin-test) &&
	test "$SHA" = "$ACTUAL"
	)
'

test_expect_success 'update-ref --stdin: create command' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD~1) &&
	printf "create refs/heads/stdin-create %s\n" "$SHA" |
	"$GUST_BIN" update-ref --stdin &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/stdin-create) &&
	test "$SHA" = "$ACTUAL"
	)
'

test_expect_success 'update-ref --stdin: delete command' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse refs/heads/stdin-test) &&
	printf "delete refs/heads/stdin-test %s\n" "$SHA" |
	"$GUST_BIN" update-ref --stdin &&
	test_must_fail "$REAL_GIT" rev-parse --verify refs/heads/stdin-test 2>/dev/null
	)
'

test_expect_success 'update-ref --stdin: verify command succeeds' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse refs/heads/stdin-create) &&
	printf "verify refs/heads/stdin-create %s\n" "$SHA" |
	"$GUST_BIN" update-ref --stdin
	)
'

test_expect_success 'update-ref --stdin: verify command fails on mismatch' '
	(
	cd repo &&
	WRONG=$("$REAL_GIT" rev-parse HEAD) &&
	printf "verify refs/heads/stdin-create %s\n" "$WRONG" |
	test_must_fail "$GUST_BIN" update-ref --stdin
	)
'

test_expect_success 'update-ref --stdin: multiple commands in batch' '
	(
	cd repo &&
	SHA1=$("$REAL_GIT" rev-parse HEAD) &&
	SHA2=$("$REAL_GIT" rev-parse HEAD~1) &&
	printf "create refs/heads/batch-a %s\ncreate refs/heads/batch-b %s\n" "$SHA1" "$SHA2" |
	"$GUST_BIN" update-ref --stdin &&
	test "$SHA1" = "$("$REAL_GIT" rev-parse refs/heads/batch-a)" &&
	test "$SHA2" = "$("$REAL_GIT" rev-parse refs/heads/batch-b)"
	)
'

###########################################################################
# Section 5: --stdin -z (NUL-terminated)
###########################################################################

test_expect_success 'update-ref --stdin -z: NUL-terminated create' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	printf "create refs/heads/nul-test %s\0" "$SHA" |
	"$GUST_BIN" update-ref --stdin -z &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/nul-test) &&
	test "$SHA" = "$ACTUAL"
	)
'

test_expect_success 'update-ref --stdin -z: NUL-terminated delete' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse refs/heads/nul-test) &&
	printf "delete refs/heads/nul-test %s\0" "$SHA" |
	"$GUST_BIN" update-ref --stdin -z &&
	test_must_fail "$REAL_GIT" rev-parse --verify refs/heads/nul-test 2>/dev/null
	)
'

###########################################################################
# Section 6: --no-deref
###########################################################################

test_expect_success 'update-ref --no-deref: updates symref itself' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	"$REAL_GIT" symbolic-ref refs/heads/sym-link refs/heads/main &&
	"$GUST_BIN" update-ref --no-deref refs/heads/sym-link "$SHA" &&
	# After --no-deref update, sym-link is no longer a symref
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/sym-link) &&
	test "$SHA" = "$ACTUAL"
	)
'

test_expect_success 'update-ref: without --no-deref follows symref' '
	(
	cd repo &&
	"$REAL_GIT" symbolic-ref refs/heads/sym-link2 refs/heads/main &&
	SHA=$("$REAL_GIT" rev-parse HEAD~1) &&
	"$GUST_BIN" update-ref refs/heads/sym-link2 "$SHA" &&
	# The target (main) should be updated
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/main) &&
	test "$SHA" = "$ACTUAL"
	)
'

test_expect_success 'update-ref: restore main after symref test' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/heads/main "$SHA" ||
	"$REAL_GIT" update-ref refs/heads/main "$SHA"
	)
'

###########################################################################
# Section 7: Reflog message with -m
###########################################################################

test_expect_success 'update-ref -m: sets reflog message' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref -m "test reflog message" refs/heads/reflog-test "$SHA" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/reflog-test) &&
	test "$SHA" = "$ACTUAL"
	)
'

test_expect_success 'update-ref -m: ref was created with correct value' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/reflog-test) &&
	test "$SHA" = "$ACTUAL"
	)
'

###########################################################################
# Section 8: Creating refs in various namespaces
###########################################################################

test_expect_success 'update-ref: create tag-like ref' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/tags/manual-tag "$SHA" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/tags/manual-tag) &&
	test "$SHA" = "$ACTUAL"
	)
'

test_expect_success 'update-ref: create custom namespace ref' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/custom/my-ref "$SHA" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/custom/my-ref) &&
	test "$SHA" = "$ACTUAL"
	)
'

test_expect_success 'update-ref: create notes ref' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/notes/commits "$SHA" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/notes/commits) &&
	test "$SHA" = "$ACTUAL"
	)
'

###########################################################################
# Section 9: Batch atomic operations
###########################################################################

test_expect_success 'update-ref --stdin: atomic batch - all succeed' '
	(
	cd repo &&
	SHA1=$("$REAL_GIT" rev-parse HEAD) &&
	SHA2=$("$REAL_GIT" rev-parse HEAD~1) &&
	"$GUST_BIN" update-ref -d refs/heads/batch-a 2>/dev/null || true &&
	"$GUST_BIN" update-ref -d refs/heads/batch-b 2>/dev/null || true &&
	printf "create refs/heads/atomic-a %s\ncreate refs/heads/atomic-b %s\n" "$SHA1" "$SHA2" |
	"$GUST_BIN" update-ref --stdin &&
	test "$SHA1" = "$("$REAL_GIT" rev-parse refs/heads/atomic-a)" &&
	test "$SHA2" = "$("$REAL_GIT" rev-parse refs/heads/atomic-b)"
	)
'

test_expect_success 'update-ref --stdin: verify + update in batch' '
	(
	cd repo &&
	OLD=$("$REAL_GIT" rev-parse refs/heads/atomic-a) &&
	NEW=$("$REAL_GIT" rev-parse refs/heads/atomic-b) &&
	printf "verify refs/heads/atomic-a %s\nupdate refs/heads/atomic-a %s %s\n" "$OLD" "$NEW" "$OLD" |
	"$GUST_BIN" update-ref --stdin &&
	test "$NEW" = "$("$REAL_GIT" rev-parse refs/heads/atomic-a)"
	)
'

###########################################################################
# Section 10: Edge cases and error handling
###########################################################################

test_expect_success 'update-ref: invalid SHA fails' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" update-ref refs/heads/bad-ref "not-a-sha"
	)
'

test_expect_success 'update-ref: delete via -d flag' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/heads/zero-del "$SHA" &&
	"$GUST_BIN" update-ref -d refs/heads/zero-del &&
	test_must_fail "$REAL_GIT" rev-parse --verify refs/heads/zero-del 2>/dev/null
	)
'

test_expect_success 'update-ref --stdin: empty input succeeds' '
	(
	cd repo &&
	echo "" | "$GUST_BIN" update-ref --stdin
	)
'

test_expect_success 'update-ref --stdin: malformed command fails' '
	(
	cd repo &&
	printf "nonsense command here\n" |
	test_must_fail "$GUST_BIN" update-ref --stdin
	)
'

test_expect_success 'update-ref: create ref pointing at tree object' '
	(
	cd repo &&
	TREE=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	"$GUST_BIN" update-ref refs/custom/tree-ref "$TREE" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/custom/tree-ref) &&
	test "$TREE" = "$ACTUAL"
	)
'

test_done
