#!/bin/sh
# Tests for grit update-ref with reflog messages, delete, no-deref, old-value check, stdin.

test_description='grit update-ref: basic, -d, --no-deref, -m, old-value, --stdin'

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
	echo "second" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit" &&
	echo "third" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "third commit"
	)
'

###########################################################################
# Section 2: Basic update-ref
###########################################################################

test_expect_success 'update-ref: create a new ref' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/heads/newbranch "$HEAD" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/newbranch) &&
	test "$HEAD" = "$ACTUAL"
	)
'

test_expect_success 'update-ref: move ref to different commit' '
	(
	cd repo &&
	FIRST=$("$REAL_GIT" rev-parse HEAD~2) &&
	"$GUST_BIN" update-ref refs/heads/newbranch "$FIRST" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/newbranch) &&
	test "$FIRST" = "$ACTUAL"
	)
'

test_expect_success 'update-ref: update ref back to HEAD' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/heads/newbranch "$HEAD" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/newbranch) &&
	test "$HEAD" = "$ACTUAL"
	)
'

test_expect_success 'update-ref: create a tag ref' '
	(
	cd repo &&
	FIRST=$("$REAL_GIT" rev-parse HEAD~2) &&
	"$GUST_BIN" update-ref refs/tags/mytag "$FIRST" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/tags/mytag) &&
	test "$FIRST" = "$ACTUAL"
	)
'

test_expect_success 'update-ref: create custom namespace ref' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/custom/myref "$HEAD" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/custom/myref) &&
	test "$HEAD" = "$ACTUAL"
	)
'

###########################################################################
# Section 3: -d (delete)
###########################################################################

test_expect_success 'update-ref -d: delete a ref' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/heads/to-delete "$HEAD" &&
	"$REAL_GIT" rev-parse refs/heads/to-delete &&
	"$GUST_BIN" update-ref -d refs/heads/to-delete &&
	test_must_fail "$REAL_GIT" rev-parse --verify refs/heads/to-delete
	)
'

test_expect_success 'update-ref -d: delete nonexistent ref succeeds silently' '
	(
	cd repo &&
	"$GUST_BIN" update-ref -d refs/heads/no-such-ref
	)
'

test_expect_success 'update-ref -d: delete tag ref' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/tags/temp-tag "$HEAD" &&
	"$GUST_BIN" update-ref -d refs/tags/temp-tag &&
	test_must_fail "$REAL_GIT" rev-parse --verify refs/tags/temp-tag
	)
'

###########################################################################
# Section 4: Old value verification
###########################################################################

test_expect_success 'update-ref: old value matches succeeds' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	FIRST=$("$REAL_GIT" rev-parse HEAD~2) &&
	"$GUST_BIN" update-ref refs/heads/oldval-test "$HEAD" &&
	"$GUST_BIN" update-ref refs/heads/oldval-test "$FIRST" "$HEAD" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/oldval-test) &&
	test "$FIRST" = "$ACTUAL"
	)
'

test_expect_success 'update-ref: old value mismatch fails' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	FIRST=$("$REAL_GIT" rev-parse HEAD~2) &&
	SECOND=$("$REAL_GIT" rev-parse HEAD~1) &&
	"$GUST_BIN" update-ref refs/heads/oldval-test2 "$HEAD" &&
	test_must_fail "$GUST_BIN" update-ref refs/heads/oldval-test2 "$FIRST" "$SECOND"
	)
'

test_expect_success 'update-ref: old value check with zero SHA means ref must not exist' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref -d refs/heads/brand-new 2>/dev/null
	"$GUST_BIN" update-ref refs/heads/brand-new "$HEAD" 0000000000000000000000000000000000000000 &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/brand-new) &&
	test "$HEAD" = "$ACTUAL"
	)
'

test_expect_success 'update-ref: old value zero SHA fails when ref exists' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	FIRST=$("$REAL_GIT" rev-parse HEAD~2) &&
	"$GUST_BIN" update-ref refs/heads/exists-check "$HEAD" &&
	test_must_fail "$GUST_BIN" update-ref refs/heads/exists-check "$FIRST" 0000000000000000000000000000000000000000
	)
'

###########################################################################
# Section 5: -m (reflog message)
###########################################################################

test_expect_success 'update-ref -m: creates ref with message option' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref -m "test log message" refs/heads/logged "$HEAD" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/logged) &&
	test "$HEAD" = "$ACTUAL"
	)
'

test_expect_success 'update-ref -m: moves ref with message' '
	(
	cd repo &&
	FIRST=$("$REAL_GIT" rev-parse HEAD~2) &&
	"$GUST_BIN" update-ref -m "moving ref" refs/heads/logged "$FIRST" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/logged) &&
	test "$FIRST" = "$ACTUAL"
	)
'

test_expect_success 'update-ref: without -m still works' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/heads/nolog "$HEAD" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/nolog) &&
	test "$HEAD" = "$ACTUAL"
	)
'

###########################################################################
# Section 6: --no-deref
###########################################################################

test_expect_success 'update-ref --no-deref: updates symbolic ref target directly' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	FIRST=$("$REAL_GIT" rev-parse HEAD~2) &&
	"$REAL_GIT" symbolic-ref refs/heads/symlink refs/heads/main &&
	"$GUST_BIN" update-ref --no-deref refs/heads/symlink "$FIRST" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/symlink) &&
	test "$FIRST" = "$ACTUAL" &&
	MAIN_ACTUAL=$("$REAL_GIT" rev-parse refs/heads/main) &&
	test "$MAIN_ACTUAL" = "$HEAD"
	)
'

test_expect_success 'update-ref: without --no-deref follows symbolic ref' '
	(
	cd repo &&
	FIRST=$("$REAL_GIT" rev-parse HEAD~2) &&
	"$REAL_GIT" symbolic-ref refs/heads/symlink2 refs/heads/main &&
	"$GUST_BIN" update-ref refs/heads/symlink2 "$FIRST" &&
	MAIN_ACTUAL=$("$REAL_GIT" rev-parse refs/heads/main) &&
	test "$MAIN_ACTUAL" = "$FIRST"
	)
'

test_expect_success 'update-ref: restore main after deref test' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD~0) &&
	THIRD=$("$REAL_GIT" log --all --oneline --format=%H | head -1) &&
	"$GUST_BIN" update-ref refs/heads/main "$THIRD"
	)
'

###########################################################################
# Section 7: --stdin
###########################################################################

test_expect_success 'update-ref --stdin: create ref' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	echo "create refs/heads/stdin-test $HEAD" | "$GUST_BIN" update-ref --stdin &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/stdin-test) &&
	test "$HEAD" = "$ACTUAL"
	)
'

test_expect_success 'update-ref --stdin: update ref' '
	(
	cd repo &&
	FIRST=$("$REAL_GIT" rev-parse HEAD~2) &&
	HEAD=$("$REAL_GIT" rev-parse refs/heads/stdin-test) &&
	printf "update refs/heads/stdin-test %s %s\n" "$FIRST" "$HEAD" | "$GUST_BIN" update-ref --stdin &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/stdin-test) &&
	test "$FIRST" = "$ACTUAL"
	)
'

test_expect_success 'update-ref --stdin: delete ref' '
	(
	cd repo &&
	echo "delete refs/heads/stdin-test" | "$GUST_BIN" update-ref --stdin &&
	test_must_fail "$REAL_GIT" rev-parse --verify refs/heads/stdin-test
	)
'

test_expect_success 'update-ref --stdin: multiple commands' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	FIRST=$("$REAL_GIT" rev-parse HEAD~2) &&
	printf "create refs/heads/stdin-a %s\ncreate refs/heads/stdin-b %s\n" "$HEAD" "$FIRST" | \
		"$GUST_BIN" update-ref --stdin &&
	A=$("$REAL_GIT" rev-parse refs/heads/stdin-a) &&
	B=$("$REAL_GIT" rev-parse refs/heads/stdin-b) &&
	test "$HEAD" = "$A" &&
	test "$FIRST" = "$B"
	)
'

###########################################################################
# Section 8: --stdin -z (NUL-terminated)
###########################################################################

test_expect_success 'update-ref --stdin -z: create ref with NUL terminators' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	printf "create refs/heads/stdin-z %s\0" "$HEAD" | "$GUST_BIN" update-ref --stdin -z &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/stdin-z) &&
	test "$HEAD" = "$ACTUAL"
	)
'

###########################################################################
# Section 9: Edge cases
###########################################################################

test_expect_success 'update-ref: no arguments fails' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" update-ref
	)
'

test_expect_success 'update-ref: invalid SHA fails' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" update-ref refs/heads/bad-sha notavalidsha
	)
'

test_expect_success 'update-ref: invalid SHA string is rejected' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" update-ref refs/heads/bad-sha notavalidsha
	)
'

test_expect_success 'update-ref: create deeply nested ref' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/a/b/c/d/e "$HEAD" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/a/b/c/d/e) &&
	test "$HEAD" = "$ACTUAL"
	)
'

###########################################################################
# Section 10: Verify state
###########################################################################

test_expect_success 'update-ref: ref visible via show-ref' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" update-ref refs/heads/visible-test "$HEAD" &&
	"$REAL_GIT" show-ref refs/heads/visible-test | grep "$HEAD"
	)
'

test_expect_success 'update-ref: deleted ref not in show-ref' '
	(
	cd repo &&
	"$GUST_BIN" update-ref -d refs/heads/visible-test &&
	! "$REAL_GIT" show-ref refs/heads/visible-test
	)
'

test_expect_success 'update-ref -m: multiple updates with messages' '
	(
	cd repo &&
	HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	FIRST=$("$REAL_GIT" rev-parse HEAD~2) &&
	"$GUST_BIN" update-ref -m "create it" refs/heads/multi-log "$HEAD" &&
	"$GUST_BIN" update-ref -m "move it" refs/heads/multi-log "$FIRST" &&
	ACTUAL=$("$REAL_GIT" rev-parse refs/heads/multi-log) &&
	test "$FIRST" = "$ACTUAL"
	)
'

test_done
