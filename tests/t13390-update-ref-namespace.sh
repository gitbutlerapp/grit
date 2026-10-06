#!/bin/sh
# Tests for grit update-ref with custom namespaces, --stdin, --no-deref, etc.

test_description='grit update-ref namespace and advanced usage'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup: create repo with initial commit' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "hello" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "first commit" &&
	echo "world" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit"
	)
'

###########################################################################
# Basic update-ref
###########################################################################

test_expect_success 'update-ref creates a new ref' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 grit update-ref refs/heads/newbranch "$HEAD") &&
	(cd repo && "$REAL_GIT" show-ref refs/heads/newbranch >../actual) &&
	test_line_count = 1 actual
'

test_expect_success 'created ref points to correct commit' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 REF=$("$REAL_GIT" rev-parse refs/heads/newbranch) &&
	 test "$HEAD" = "$REF")
'

test_expect_success 'update-ref can update existing ref' '
	(cd repo &&
	 FIRST=$("$REAL_GIT" rev-parse HEAD~1) &&
	 grit update-ref refs/heads/newbranch "$FIRST") &&
	(cd repo &&
	 FIRST=$("$REAL_GIT" rev-parse HEAD~1) &&
	 REF=$("$REAL_GIT" rev-parse refs/heads/newbranch) &&
	 test "$FIRST" = "$REF")
'

###########################################################################
# Custom namespaces
###########################################################################

test_expect_success 'update-ref creates ref in custom namespace' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 grit update-ref refs/custom/myref "$HEAD") &&
	(cd repo && "$REAL_GIT" show-ref refs/custom/myref >../actual) &&
	grep "refs/custom/myref" actual
'

test_expect_success 'update-ref in refs/notes namespace' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 grit update-ref refs/notes/commits "$HEAD") &&
	(cd repo && "$REAL_GIT" show-ref refs/notes/commits >../actual) &&
	grep "refs/notes/commits" actual
'

test_expect_success 'update-ref in refs/stash namespace' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 grit update-ref refs/stash "$HEAD") &&
	(cd repo && "$REAL_GIT" show-ref refs/stash >../actual) &&
	grep "refs/stash" actual
'

test_expect_success 'update-ref in deeply nested namespace' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 grit update-ref refs/custom/deep/nested/ref "$HEAD") &&
	(cd repo && "$REAL_GIT" show-ref refs/custom/deep/nested/ref >../actual) &&
	grep "refs/custom/deep/nested/ref" actual
'

test_expect_success 'multiple refs in same custom namespace' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 FIRST=$("$REAL_GIT" rev-parse HEAD~1) &&
	 grit update-ref refs/custom/ref-a "$HEAD" &&
	 grit update-ref refs/custom/ref-b "$FIRST") &&
	(cd repo && grit show-ref >../actual) &&
	grep "refs/custom/ref-a" actual &&
	grep "refs/custom/ref-b" actual
'

###########################################################################
# Delete ref with -d
###########################################################################

test_expect_success 'update-ref -d deletes a ref' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 grit update-ref refs/temp/deleteme "$HEAD" &&
	 "$REAL_GIT" show-ref refs/temp/deleteme &&
	 grit update-ref -d refs/temp/deleteme) &&
	(cd repo && test_must_fail "$REAL_GIT" show-ref refs/temp/deleteme)
'

test_expect_success 'update-ref -d on nonexistent ref is silent' '
	(cd repo &&
	 grit update-ref -d refs/temp/nonexistent)
'

test_expect_success 'update-ref -d on custom namespace ref' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 grit update-ref refs/custom/temp "$HEAD" &&
	 grit update-ref -d refs/custom/temp) &&
	(cd repo && test_must_fail "$REAL_GIT" show-ref --verify refs/custom/temp)
'

###########################################################################
# Old value verification
###########################################################################

test_expect_success 'update-ref with correct old value succeeds' '
	(cd repo &&
	 FIRST=$("$REAL_GIT" rev-parse HEAD~1) &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 grit update-ref refs/heads/newbranch "$HEAD" "$FIRST")
'

test_expect_success 'update-ref with wrong old value fails' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 test_must_fail grit update-ref refs/heads/newbranch "$HEAD" 0000000000000000000000000000000000000001)
'

test_expect_success 'ref unchanged after old value mismatch' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 REF=$("$REAL_GIT" rev-parse refs/heads/newbranch) &&
	 test "$HEAD" = "$REF")
'

###########################################################################
# --no-deref
###########################################################################

test_expect_success 'update-ref --no-deref creates ref directly' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 grit update-ref --no-deref refs/heads/noderef-test "$HEAD") &&
	(cd repo && "$REAL_GIT" rev-parse refs/heads/noderef-test >../actual) &&
	(cd repo && "$REAL_GIT" rev-parse HEAD >../expect) &&
	test_cmp expect actual
'

###########################################################################
# --stdin batch mode
###########################################################################

test_expect_success 'update-ref --stdin create command' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 echo "create refs/stdin/test1 $HEAD" | grit update-ref --stdin) &&
	(cd repo && "$REAL_GIT" show-ref refs/stdin/test1 >../actual) &&
	grep "refs/stdin/test1" actual
'

test_expect_success 'update-ref --stdin update command' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 FIRST=$("$REAL_GIT" rev-parse HEAD~1) &&
	 echo "update refs/stdin/test1 $FIRST $HEAD" | grit update-ref --stdin) &&
	(cd repo &&
	 FIRST=$("$REAL_GIT" rev-parse HEAD~1) &&
	 REF=$("$REAL_GIT" rev-parse refs/stdin/test1) &&
	 test "$FIRST" = "$REF")
'

test_expect_success 'update-ref --stdin delete command' '
	(cd repo &&
	 FIRST=$("$REAL_GIT" rev-parse HEAD~1) &&
	 echo "delete refs/stdin/test1 $FIRST" | grit update-ref --stdin) &&
	(cd repo && test_must_fail "$REAL_GIT" show-ref --verify refs/stdin/test1)
'

test_expect_success 'update-ref --stdin multiple commands' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 FIRST=$("$REAL_GIT" rev-parse HEAD~1) &&
	 printf "create refs/stdin/multi-a %s\ncreate refs/stdin/multi-b %s\n" "$HEAD" "$FIRST" |
	 grit update-ref --stdin) &&
	(cd repo && "$REAL_GIT" show-ref refs/stdin/multi-a >../actual) &&
	grep "refs/stdin/multi-a" actual &&
	(cd repo && "$REAL_GIT" show-ref refs/stdin/multi-b >../actual) &&
	grep "refs/stdin/multi-b" actual
'

test_expect_success 'update-ref --stdin with verify command' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 echo "verify refs/stdin/multi-a $HEAD" | grit update-ref --stdin)
'

test_expect_success 'update-ref --stdin verify with wrong SHA fails' '
	(cd repo &&
	 echo "verify refs/stdin/multi-a 0000000000000000000000000000000000000001" |
	 test_must_fail grit update-ref --stdin)
'

###########################################################################
# Reflog message with -m
###########################################################################

test_expect_success 'update-ref -m sets reflog message' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 grit update-ref -m "my reflog msg" refs/heads/logged "$HEAD")
'

###########################################################################
# Edge cases
###########################################################################

test_expect_success 'update-ref with no args fails' '
	(cd repo && test_must_fail grit update-ref)
'

test_expect_success 'update-ref to zero SHA deletes ref' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 grit update-ref refs/temp/zero-del "$HEAD" &&
	 grit update-ref refs/temp/zero-del 0000000000000000000000000000000000000000) &&
	(cd repo && test_must_fail "$REAL_GIT" show-ref --verify refs/temp/zero-del)
'

test_expect_success 'update-ref matches git behavior for branch creation' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 grit update-ref refs/heads/grit-created "$HEAD" &&
	 GRIT_VAL=$("$REAL_GIT" rev-parse refs/heads/grit-created) &&
	 test "$GRIT_VAL" = "$HEAD")
'

test_expect_success 'update-ref to tags namespace works' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 grit update-ref refs/tags/manually-created "$HEAD") &&
	(cd repo && "$REAL_GIT" show-ref refs/tags/manually-created >../actual) &&
	grep "refs/tags/manually-created" actual
'

test_expect_success 'update-ref --stdin create in custom namespace' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 echo "create refs/custom/stdin-ns $HEAD" | grit update-ref --stdin) &&
	(cd repo && grit show-ref >../actual) &&
	grep "refs/custom/stdin-ns" actual
'

test_expect_success 'update-ref overwrites with same value is idempotent' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 grit update-ref refs/heads/idempotent "$HEAD" &&
	 grit update-ref refs/heads/idempotent "$HEAD") &&
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 REF=$("$REAL_GIT" rev-parse refs/heads/idempotent) &&
	 test "$HEAD" = "$REF")
'

test_expect_success 'cleanup: verify all custom refs are accessible via show-ref' '
	(cd repo && grit show-ref >../actual) &&
	grep "refs/custom/" actual &&
	grep "refs/heads/" actual
'

test_done
