#!/bin/sh
# Tests for grit symbolic-ref in regular, bare, and worktree repos.

test_description='grit symbolic-ref: read, create, delete, --short, --quiet, -d, bare, worktree'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create regular repo' '
	(
	"$REAL_GIT" init -b main repo &&
	cd "$TRASH_DIRECTORY"/repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "hello" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial commit" &&
	"$REAL_GIT" branch feature &&
	"$REAL_GIT" branch develop &&
	echo "more" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit"
	)
'

###########################################################################
# Section 2: Read HEAD symbolic ref
###########################################################################

test_expect_success 'symbolic-ref: read HEAD' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$GUST_BIN" symbolic-ref HEAD >actual &&
	"$REAL_GIT" symbolic-ref HEAD >expected &&
	test_cmp expected actual
'

test_expect_success 'symbolic-ref: HEAD points to refs/heads/main' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$GUST_BIN" symbolic-ref HEAD >actual &&
	grep "refs/heads/main" actual
'

test_expect_success 'symbolic-ref --short HEAD: shows short name' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$GUST_BIN" symbolic-ref --short HEAD >actual &&
	"$REAL_GIT" symbolic-ref --short HEAD >expected &&
	test_cmp expected actual
'

test_expect_success 'symbolic-ref --short HEAD: is just branch name' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$GUST_BIN" symbolic-ref --short HEAD >actual &&
	echo "main" >expected &&
	test_cmp expected actual
'

###########################################################################
# Section 3: Create symbolic ref
###########################################################################

test_expect_success 'symbolic-ref: create new symbolic ref' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$GUST_BIN" symbolic-ref refs/heads/alias refs/heads/feature &&
	"$GUST_BIN" symbolic-ref refs/heads/alias >actual &&
	echo "refs/heads/feature" >expected &&
	test_cmp expected actual
'

test_expect_success 'symbolic-ref: update symbolic ref target' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$GUST_BIN" symbolic-ref refs/heads/alias refs/heads/develop &&
	"$GUST_BIN" symbolic-ref refs/heads/alias >actual &&
	echo "refs/heads/develop" >expected &&
	test_cmp expected actual
'

test_expect_success 'symbolic-ref: set HEAD to feature' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$GUST_BIN" symbolic-ref HEAD refs/heads/feature &&
	"$GUST_BIN" symbolic-ref HEAD >actual &&
	echo "refs/heads/feature" >expected &&
	test_cmp expected actual
'

test_expect_success 'symbolic-ref: restore HEAD to main' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$GUST_BIN" symbolic-ref HEAD refs/heads/main &&
	"$GUST_BIN" symbolic-ref HEAD >actual &&
	echo "refs/heads/main" >expected &&
	test_cmp expected actual
'

###########################################################################
# Section 4: Delete symbolic ref
###########################################################################

test_expect_success 'symbolic-ref -d: delete symbolic ref' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$GUST_BIN" symbolic-ref refs/heads/to-delete refs/heads/main &&
	"$GUST_BIN" symbolic-ref refs/heads/to-delete >actual &&
	grep refs/heads/main actual &&
	"$GUST_BIN" symbolic-ref -d refs/heads/to-delete &&
	test_must_fail "$GUST_BIN" symbolic-ref refs/heads/to-delete
'

test_expect_success 'symbolic-ref --delete: long form' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$GUST_BIN" symbolic-ref refs/heads/del2 refs/heads/main &&
	"$GUST_BIN" symbolic-ref --delete refs/heads/del2 &&
	test_must_fail "$GUST_BIN" symbolic-ref refs/heads/del2
'

###########################################################################
# Section 5: --quiet
###########################################################################

test_expect_success 'symbolic-ref --quiet: no error output on non-symbolic ref' '
	cd "$TRASH_DIRECTORY"/repo &&
	test_must_fail "$GUST_BIN" symbolic-ref --quiet refs/heads/main 2>err &&
	test ! -s err
'

test_expect_success 'symbolic-ref: without --quiet shows error on non-symbolic' '
	cd "$TRASH_DIRECTORY"/repo &&
	test_must_fail "$GUST_BIN" symbolic-ref refs/heads/main 2>err &&
	test -s err
'

test_expect_success 'symbolic-ref -q: short form suppresses error' '
	cd "$TRASH_DIRECTORY"/repo &&
	test_must_fail "$GUST_BIN" symbolic-ref -q refs/heads/main 2>err &&
	test ! -s err
'

###########################################################################
# Section 6: Bare repo
###########################################################################

test_expect_success 'setup: create bare repo' '
	cd "$TRASH_DIRECTORY" &&
	"$REAL_GIT" clone --bare repo bare.git
'

test_expect_success 'symbolic-ref: read HEAD in bare repo' '
	cd "$TRASH_DIRECTORY"/bare.git &&
	"$GUST_BIN" symbolic-ref HEAD >actual &&
	"$REAL_GIT" symbolic-ref HEAD >expected &&
	test_cmp expected actual
'

test_expect_success 'symbolic-ref --short HEAD in bare repo' '
	cd "$TRASH_DIRECTORY"/bare.git &&
	"$GUST_BIN" symbolic-ref --short HEAD >actual &&
	"$REAL_GIT" symbolic-ref --short HEAD >expected &&
	test_cmp expected actual
'

test_expect_success 'symbolic-ref: set HEAD in bare repo' '
	cd "$TRASH_DIRECTORY"/bare.git &&
	"$GUST_BIN" symbolic-ref HEAD refs/heads/feature &&
	"$GUST_BIN" symbolic-ref HEAD >actual &&
	echo "refs/heads/feature" >expected &&
	test_cmp expected actual
'

test_expect_success 'symbolic-ref: restore HEAD in bare repo' '
	cd "$TRASH_DIRECTORY"/bare.git &&
	"$GUST_BIN" symbolic-ref HEAD refs/heads/main &&
	"$GUST_BIN" symbolic-ref HEAD >actual &&
	echo "refs/heads/main" >expected &&
	test_cmp expected actual
'

test_expect_success 'symbolic-ref: create custom symref in bare repo' '
	cd "$TRASH_DIRECTORY"/bare.git &&
	"$GUST_BIN" symbolic-ref refs/heads/bare-alias refs/heads/develop &&
	"$GUST_BIN" symbolic-ref refs/heads/bare-alias >actual &&
	echo "refs/heads/develop" >expected &&
	test_cmp expected actual
'

###########################################################################
# Section 7: Worktree
###########################################################################

test_expect_success 'setup: create worktree' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$REAL_GIT" worktree add ../wt feature
'

test_expect_success 'symbolic-ref: HEAD in worktree' '
	cd "$TRASH_DIRECTORY"/wt &&
	"$GUST_BIN" symbolic-ref HEAD >actual &&
	echo "refs/heads/feature" >expected &&
	test_cmp expected actual
'

test_expect_success 'symbolic-ref --short: HEAD in worktree' '
	cd "$TRASH_DIRECTORY"/wt &&
	"$GUST_BIN" symbolic-ref --short HEAD >actual &&
	echo "feature" >expected &&
	test_cmp expected actual
'

test_expect_success 'symbolic-ref: main repo HEAD unchanged by worktree' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$GUST_BIN" symbolic-ref HEAD >actual &&
	echo "refs/heads/main" >expected &&
	test_cmp expected actual
'

###########################################################################
# Section 8: -m message
###########################################################################

test_expect_success 'symbolic-ref -m: set with message' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$GUST_BIN" symbolic-ref -m "switching branch" HEAD refs/heads/develop &&
	"$GUST_BIN" symbolic-ref HEAD >actual &&
	echo "refs/heads/develop" >expected &&
	test_cmp expected actual
'

test_expect_success 'symbolic-ref -m: restore HEAD' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$GUST_BIN" symbolic-ref -m "restore" HEAD refs/heads/main &&
	"$GUST_BIN" symbolic-ref HEAD >actual &&
	echo "refs/heads/main" >expected &&
	test_cmp expected actual
'

###########################################################################
# Section 9: --no-recurse
###########################################################################

test_expect_success 'symbolic-ref --no-recurse: stops at first deref' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$GUST_BIN" symbolic-ref refs/heads/chain1 refs/heads/main &&
	"$GUST_BIN" symbolic-ref refs/heads/chain2 refs/heads/chain1 &&
	"$GUST_BIN" symbolic-ref --no-recurse refs/heads/chain2 >actual &&
	echo "refs/heads/chain1" >expected &&
	test_cmp expected actual
'

###########################################################################
# Section 10: Edge cases
###########################################################################

test_expect_success 'symbolic-ref: no arguments fails' '
	cd "$TRASH_DIRECTORY"/repo &&
	test_must_fail "$GUST_BIN" symbolic-ref
'

test_expect_success 'symbolic-ref: nonexistent ref fails' '
	cd "$TRASH_DIRECTORY"/repo &&
	test_must_fail "$GUST_BIN" symbolic-ref refs/heads/nonexistent
'

test_expect_success 'symbolic-ref -d: delete nonexistent fails' '
	cd "$TRASH_DIRECTORY"/repo &&
	test_must_fail "$GUST_BIN" symbolic-ref -d refs/heads/nonexistent
'

test_expect_success 'symbolic-ref: matches git output on HEAD' '
	cd "$TRASH_DIRECTORY"/repo &&
	"$GUST_BIN" symbolic-ref HEAD >grit_out &&
	"$REAL_GIT" symbolic-ref HEAD >git_out &&
	test_cmp git_out grit_out
'

test_done
