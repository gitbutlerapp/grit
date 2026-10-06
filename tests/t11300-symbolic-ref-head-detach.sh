#!/bin/sh
# Tests for grit symbolic-ref: HEAD management, detached HEAD, --short, --delete, --no-recurse.

test_description='grit symbolic-ref: read/write/delete symbolic refs, HEAD, detached state, short, no-recurse'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with branches' '
	(
	"$REAL_GIT" init -b main repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&

	echo "initial" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial commit" &&

	"$REAL_GIT" branch feature &&
	"$REAL_GIT" branch develop &&

	echo "second" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit"
	)
'

###########################################################################
# Section 2: Reading symbolic refs
###########################################################################

test_expect_success 'symbolic-ref: read HEAD' '
	(
	cd repo &&
	"$GUST_BIN" symbolic-ref HEAD >output.txt &&
	grep "refs/heads/main" output.txt
	)
'

test_expect_success 'symbolic-ref: read HEAD after checkout' '
	(
	cd repo &&
	"$REAL_GIT" checkout feature &&
	"$GUST_BIN" symbolic-ref HEAD >output.txt &&
	grep "refs/heads/feature" output.txt &&
	"$REAL_GIT" checkout main
	)
'

test_expect_success 'symbolic-ref: returns full refname by default' '
	(
	cd repo &&
	RESULT=$("$GUST_BIN" symbolic-ref HEAD) &&
	test "$RESULT" = "refs/heads/main"
	)
'

###########################################################################
# Section 3: --short
###########################################################################

test_expect_success 'symbolic-ref --short: returns short branch name' '
	(
	cd repo &&
	"$GUST_BIN" symbolic-ref --short HEAD >output.txt &&
	grep "^main$" output.txt
	)
'

test_expect_success 'symbolic-ref --short: feature branch' '
	(
	cd repo &&
	"$REAL_GIT" checkout feature &&
	"$GUST_BIN" symbolic-ref --short HEAD >output.txt &&
	grep "^feature$" output.txt &&
	"$REAL_GIT" checkout main
	)
'

test_expect_success 'symbolic-ref --short: develop branch' '
	(
	cd repo &&
	"$REAL_GIT" checkout develop &&
	"$GUST_BIN" symbolic-ref --short HEAD >output.txt &&
	grep "^develop$" output.txt &&
	"$REAL_GIT" checkout main
	)
'

###########################################################################
# Section 4: Writing symbolic refs
###########################################################################

test_expect_success 'symbolic-ref: set HEAD to different branch' '
	(
	cd repo &&
	"$GUST_BIN" symbolic-ref HEAD refs/heads/feature &&
	RESULT=$("$GUST_BIN" symbolic-ref HEAD) &&
	test "$RESULT" = "refs/heads/feature"
	)
'

test_expect_success 'symbolic-ref: restore HEAD to main' '
	(
	cd repo &&
	"$GUST_BIN" symbolic-ref HEAD refs/heads/main &&
	RESULT=$("$GUST_BIN" symbolic-ref HEAD) &&
	test "$RESULT" = "refs/heads/main"
	)
'

test_expect_success 'symbolic-ref: create custom symbolic ref' '
	(
	cd repo &&
	"$GUST_BIN" symbolic-ref refs/heads/alias refs/heads/main &&
	RESULT=$("$GUST_BIN" symbolic-ref refs/heads/alias) &&
	test "$RESULT" = "refs/heads/main"
	)
'

test_expect_success 'symbolic-ref: update custom symbolic ref' '
	(
	cd repo &&
	"$GUST_BIN" symbolic-ref refs/heads/alias refs/heads/feature &&
	RESULT=$("$GUST_BIN" symbolic-ref refs/heads/alias) &&
	test "$RESULT" = "refs/heads/feature"
	)
'

###########################################################################
# Section 5: Detached HEAD
###########################################################################

test_expect_success 'symbolic-ref: fails on detached HEAD' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	"$REAL_GIT" checkout "$SHA" --detach 2>/dev/null &&
	test_must_fail "$GUST_BIN" symbolic-ref HEAD
	)
'

test_expect_success 'symbolic-ref -q: quiet on detached HEAD, still fails' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" symbolic-ref -q HEAD >output.txt 2>&1 &&
	test_must_be_empty output.txt
	)
'

test_expect_success 'symbolic-ref: reattach HEAD' '
	(
	cd repo &&
	"$GUST_BIN" symbolic-ref HEAD refs/heads/main &&
	RESULT=$("$GUST_BIN" symbolic-ref HEAD) &&
	test "$RESULT" = "refs/heads/main"
	)
'

###########################################################################
# Section 6: Delete symbolic ref (-d)
###########################################################################

test_expect_success 'symbolic-ref -d: delete custom symbolic ref' '
	(
	cd repo &&
	"$GUST_BIN" symbolic-ref refs/heads/alias refs/heads/main &&
	"$GUST_BIN" symbolic-ref -d refs/heads/alias &&
	test_must_fail "$GUST_BIN" symbolic-ref refs/heads/alias 2>/dev/null
	)
'

test_expect_success 'symbolic-ref -d: cannot delete non-symbolic ref' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" symbolic-ref -d refs/heads/main 2>err.txt
	)
'

test_expect_success 'symbolic-ref -d: delete non-existent ref fails' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" symbolic-ref -d refs/heads/no-such-symref 2>err.txt
	)
'

###########################################################################
# Section 7: --no-recurse
###########################################################################

test_expect_success 'symbolic-ref --no-recurse: stops at first level' '
	(
	cd repo &&
	"$GUST_BIN" symbolic-ref refs/sym/level1 refs/heads/main &&
	RESULT=$("$GUST_BIN" symbolic-ref --no-recurse refs/sym/level1) &&
	test "$RESULT" = "refs/heads/main"
	)
'

test_expect_success 'symbolic-ref --no-recurse: chained symrefs stop at one level' '
	(
	cd repo &&
	"$GUST_BIN" symbolic-ref refs/sym/level2 refs/sym/level1 &&
	RESULT=$("$GUST_BIN" symbolic-ref --no-recurse refs/sym/level2) &&
	test "$RESULT" = "refs/sym/level1"
	)
'

test_expect_success 'symbolic-ref: without --no-recurse resolves fully' '
	(
	cd repo &&
	RESULT=$("$GUST_BIN" symbolic-ref refs/sym/level2) &&
	test "$RESULT" = "refs/heads/main"
	)
'

###########################################################################
# Section 8: -m (reflog message)
###########################################################################

test_expect_success 'symbolic-ref -m: set with reflog message' '
	(
	cd repo &&
	"$GUST_BIN" symbolic-ref -m "switching to develop" HEAD refs/heads/develop &&
	RESULT=$("$GUST_BIN" symbolic-ref HEAD) &&
	test "$RESULT" = "refs/heads/develop"
	)
'

test_expect_success 'symbolic-ref -m: HEAD now points to develop' '
	(
	cd repo &&
	RESULT=$("$GUST_BIN" symbolic-ref HEAD) &&
	test "$RESULT" = "refs/heads/develop"
	)
'

test_expect_success 'symbolic-ref: restore HEAD to main' '
	(
	cd repo &&
	"$GUST_BIN" symbolic-ref HEAD refs/heads/main
	)
'

###########################################################################
# Section 9: Various ref targets
###########################################################################

test_expect_success 'symbolic-ref: point to tags namespace' '
	(
	cd repo &&
	"$REAL_GIT" tag v1.0 HEAD &&
	"$GUST_BIN" symbolic-ref refs/sym/tag-alias refs/tags/v1.0 &&
	RESULT=$("$GUST_BIN" symbolic-ref refs/sym/tag-alias) &&
	test "$RESULT" = "refs/tags/v1.0"
	)
'

test_expect_success 'symbolic-ref: point to custom namespace' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	"$REAL_GIT" update-ref refs/custom/target "$SHA" &&
	"$GUST_BIN" symbolic-ref refs/sym/custom-alias refs/custom/target &&
	RESULT=$("$GUST_BIN" symbolic-ref refs/sym/custom-alias) &&
	test "$RESULT" = "refs/custom/target"
	)
'

###########################################################################
# Section 10: Error handling and edge cases
###########################################################################

test_expect_success 'symbolic-ref: no arguments fails' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" symbolic-ref 2>err.txt
	)
'

test_expect_success 'symbolic-ref: reading non-ref fails' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" symbolic-ref refs/heads/nonexistent 2>err.txt
	)
'

test_expect_success 'symbolic-ref: works in fresh repo before first commit' '
	(
	"$REAL_GIT" init -b main fresh-repo &&
	cd fresh-repo &&
	RESULT=$("$GUST_BIN" symbolic-ref HEAD) &&
	# HEAD should point to refs/heads/main or refs/heads/master
	echo "$RESULT" | grep "refs/heads/"
	)
'

test_expect_success 'symbolic-ref: HEAD in bare repo' '
	(
	"$REAL_GIT" init -b main --bare bare-repo.git &&
	cd bare-repo.git &&
	RESULT=$("$GUST_BIN" symbolic-ref HEAD) &&
	echo "$RESULT" | grep "refs/heads/"
	)
'

test_expect_success 'symbolic-ref: set HEAD in bare repo' '
	(
	cd bare-repo.git &&
	"$GUST_BIN" symbolic-ref HEAD refs/heads/develop &&
	RESULT=$("$GUST_BIN" symbolic-ref HEAD) &&
	test "$RESULT" = "refs/heads/develop"
	)
'

test_expect_success 'symbolic-ref --short: works with custom refs' '
	(
	cd repo &&
	"$GUST_BIN" symbolic-ref refs/sym/short-test refs/heads/feature &&
	"$GUST_BIN" symbolic-ref --short refs/sym/short-test >output.txt &&
	grep "feature" output.txt
	)
'

test_expect_success 'symbolic-ref: overwrite existing symref' '
	(
	cd repo &&
	"$GUST_BIN" symbolic-ref refs/sym/overwrite refs/heads/main &&
	"$GUST_BIN" symbolic-ref refs/sym/overwrite refs/heads/develop &&
	RESULT=$("$GUST_BIN" symbolic-ref refs/sym/overwrite) &&
	test "$RESULT" = "refs/heads/develop"
	)
'

test_done
