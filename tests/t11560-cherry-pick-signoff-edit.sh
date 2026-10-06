#!/bin/sh
# Tests for grit cherry-pick with --signoff, -n (no-commit), -x, and message handling.

test_description='grit cherry-pick: --signoff, -n, -x, message manipulation'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with multiple branches' '
	(
	"$REAL_GIT" init -b main repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "base" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial commit" &&
	"$REAL_GIT" branch base &&
	echo "line2" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit" &&
	echo "line3" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "third commit" &&
	"$REAL_GIT" checkout -b feature base &&
	echo "feature line" >feature.txt &&
	"$REAL_GIT" add feature.txt &&
	"$REAL_GIT" commit -m "add feature file" &&
	echo "feature2" >>feature.txt &&
	"$REAL_GIT" add feature.txt &&
	"$REAL_GIT" commit -m "update feature file" &&
	echo "feature3" >>feature.txt &&
	"$REAL_GIT" add feature.txt &&
	"$REAL_GIT" commit -m "finalize feature file" &&
	"$REAL_GIT" checkout main
	)
'

# Helper to reset to a clean state
# Usage: clean_checkout <branch> <start-point>
clean_checkout () {
	"$REAL_GIT" cherry-pick --abort 2>/dev/null
	"$REAL_GIT" reset --hard 2>/dev/null
	"$REAL_GIT" checkout -B "$1" "$2"
}

###########################################################################
# Section 2: Basic cherry-pick
###########################################################################

test_expect_success 'cherry-pick: basic single commit' '
	(
	cd repo &&
	clean_checkout test1 base &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick "$PICK" &&
	test -f feature.txt &&
	"$REAL_GIT" log --oneline -1 | grep "add feature file"
	)
'

test_expect_success 'cherry-pick: commit message is preserved' '
	(
	cd repo &&
	clean_checkout test2 base &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick "$PICK" &&
	"$REAL_GIT" log -1 --format=%s | grep "add feature file"
	)
'

test_expect_success 'cherry-pick: author is preserved' '
	(
	cd repo &&
	clean_checkout test3 base &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick "$PICK" &&
	"$REAL_GIT" log -1 --format=%an | grep "Test User"
	)
'

test_expect_success 'cherry-pick: creates new commit onto diverged base' '
	(
	cd repo &&
	clean_checkout test4 main &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick "$PICK" &&
	NEW=$("$REAL_GIT" rev-parse HEAD) &&
	test "$NEW" != "$PICK"
	)
'

test_expect_success 'cherry-pick: working tree matches after pick' '
	(
	cd repo &&
	clean_checkout test5 base &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick "$PICK" &&
	echo "feature line" >expected &&
	test_cmp expected feature.txt
	)
'

test_expect_success 'cherry-pick: file is tracked after pick' '
	(
	cd repo &&
	clean_checkout test5b base &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick "$PICK" &&
	"$REAL_GIT" ls-files | grep feature.txt
	)
'

###########################################################################
# Section 3: --signoff
###########################################################################

test_expect_success 'cherry-pick --signoff: adds Signed-off-by line' '
	(
	cd repo &&
	clean_checkout test-signoff1 base &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick --signoff "$PICK" &&
	"$REAL_GIT" log -1 --format=%B | grep "Signed-off-by:"
	)
'

test_expect_success 'cherry-pick --signoff: includes committer identity' '
	(
	cd repo &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	clean_checkout test-signoff2 base &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick --signoff "$PICK" &&
	"$REAL_GIT" log -1 --format=%B | grep "Signed-off-by: Test User <test@example.com>"
	)
'

test_expect_success 'cherry-pick -s: short form of --signoff' '
	(
	cd repo &&
	clean_checkout test-signoff3 base &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick -s "$PICK" &&
	"$REAL_GIT" log -1 --format=%B | grep "Signed-off-by:"
	)
'

test_expect_success 'cherry-pick without --signoff: no Signed-off-by' '
	(
	cd repo &&
	clean_checkout test-signoff4 base &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick "$PICK" &&
	! "$REAL_GIT" log -1 --format=%B | grep "Signed-off-by:"
	)
'

###########################################################################
# Section 4: -n / --no-commit
###########################################################################

test_expect_success 'cherry-pick -n: does not create commit' '
	(
	cd repo &&
	clean_checkout test-n1 base &&
	BEFORE=$("$REAL_GIT" rev-parse HEAD) &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick -n "$PICK" &&
	AFTER=$("$REAL_GIT" rev-parse HEAD) &&
	test "$BEFORE" = "$AFTER"
	)
'

test_expect_success 'cherry-pick --no-commit: stages changes' '
	(
	cd repo &&
	clean_checkout test-n2 base &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick --no-commit "$PICK" &&
	"$REAL_GIT" diff --cached --name-only | grep "feature.txt"
	)
'

test_expect_success 'cherry-pick -n: file content is correct' '
	(
	cd repo &&
	clean_checkout test-n3 base &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick -n "$PICK" &&
	echo "feature line" >expected &&
	test_cmp expected feature.txt
	)
'

test_expect_success 'cherry-pick -n: can manually commit afterwards' '
	(
	cd repo &&
	clean_checkout test-n4 base &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick -n "$PICK" &&
	"$REAL_GIT" commit -m "manually committed cherry-pick" &&
	"$REAL_GIT" log -1 --format=%s | grep "manually committed"
	)
'

###########################################################################
# Section 5: -x (append cherry-pick origin)
###########################################################################

test_expect_success 'cherry-pick -x: appends cherry picked from line' '
	(
	cd repo &&
	clean_checkout test-x1 main &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick -x "$PICK" &&
	"$REAL_GIT" log -1 --format=%B | grep "cherry picked from commit $PICK"
	)
'

test_expect_success 'cherry-pick without -x: no cherry picked from line' '
	(
	cd repo &&
	clean_checkout test-x2 main &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick "$PICK" &&
	! "$REAL_GIT" log -1 --format=%B | grep "cherry picked from"
	)
'

test_expect_success 'cherry-pick -x --signoff: both trailers present' '
	(
	cd repo &&
	clean_checkout test-x3 main &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick -x --signoff "$PICK" &&
	"$REAL_GIT" log -1 --format=%B | grep "cherry picked from commit" &&
	"$REAL_GIT" log -1 --format=%B | grep "Signed-off-by:"
	)
'

test_expect_success 'cherry-pick -x: reference line starts with parenthesis' '
	(
	cd repo &&
	clean_checkout test-x4 main &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick -x "$PICK" &&
	"$REAL_GIT" log -1 --format=%B | grep "^(cherry picked from commit"
	)
'

###########################################################################
# Section 6: Multiple commits in sequence
###########################################################################

test_expect_success 'cherry-pick: two commits sequentially' '
	(
	cd repo &&
	clean_checkout test-multi1 base &&
	PICK1=$("$REAL_GIT" rev-parse feature~2) &&
	PICK2=$("$REAL_GIT" rev-parse feature~1) &&
	"$GUST_BIN" cherry-pick "$PICK1" &&
	"$GUST_BIN" cherry-pick "$PICK2" &&
	"$REAL_GIT" log --oneline -2 | grep "add feature file" &&
	"$REAL_GIT" log --oneline -2 | grep "update feature file"
	)
'

test_expect_success 'cherry-pick: HEAD advances after pick' '
	(
	cd repo &&
	clean_checkout test-multi2 main &&
	H0=$("$REAL_GIT" rev-parse HEAD) &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick "$PICK" &&
	H1=$("$REAL_GIT" rev-parse HEAD) &&
	test "$H0" != "$H1"
	)
'

test_expect_success 'cherry-pick: three commits sequentially all apply' '
	(
	cd repo &&
	clean_checkout test-multi3 base &&
	PICK1=$("$REAL_GIT" rev-parse feature~2) &&
	PICK2=$("$REAL_GIT" rev-parse feature~1) &&
	PICK3=$("$REAL_GIT" rev-parse feature) &&
	"$GUST_BIN" cherry-pick "$PICK1" &&
	"$GUST_BIN" cherry-pick "$PICK2" &&
	"$GUST_BIN" cherry-pick "$PICK3" &&
	"$REAL_GIT" log --oneline -3 | wc -l | grep 3
	)
'

###########################################################################
# Section 7: cherry-pick by ref
###########################################################################

test_expect_success 'cherry-pick: by abbreviated hash' '
	(
	cd repo &&
	clean_checkout test-abbrev main &&
	FULL=$("$REAL_GIT" rev-parse feature~2) &&
	ABBREV=$(echo "$FULL" | cut -c1-8) &&
	"$GUST_BIN" cherry-pick "$ABBREV" &&
	test -f feature.txt
	)
'

###########################################################################
# Section 8: cherry-pick onto different bases
###########################################################################

test_expect_success 'cherry-pick: onto branch with extra content' '
	(
	cd repo &&
	clean_checkout test-diverge base &&
	echo "diverged" >other.txt &&
	"$REAL_GIT" add other.txt &&
	"$REAL_GIT" commit -m "diverged content" &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick "$PICK" &&
	test -f feature.txt &&
	test -f other.txt
	)
'

test_expect_success 'cherry-pick: parent is current HEAD' '
	(
	cd repo &&
	clean_checkout test-parent main &&
	PARENT_BEFORE=$("$REAL_GIT" rev-parse HEAD) &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick "$PICK" &&
	"$REAL_GIT" log -1 --format=%P HEAD | grep "$PARENT_BEFORE"
	)
'

###########################################################################
# Section 9: Edge cases
###########################################################################

test_expect_success 'cherry-pick: nonexistent commit fails' '
	(
	cd repo &&
	clean_checkout test-bad main &&
	test_must_fail "$GUST_BIN" cherry-pick 0000000000000000000000000000000000000000
	)
'

test_expect_success 'cherry-pick: no argument fails' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" cherry-pick
	)
'

test_expect_success 'cherry-pick -n: stages but no commit' '
	(
	cd repo &&
	clean_checkout test-nx main &&
	BEFORE=$("$REAL_GIT" rev-parse HEAD) &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick -n "$PICK" &&
	AFTER=$("$REAL_GIT" rev-parse HEAD) &&
	test "$BEFORE" = "$AFTER" &&
	test -f feature.txt
	)
'

###########################################################################
# Section 10: Signoff with various configs
###########################################################################

test_expect_success 'cherry-pick --signoff: uses current committer name' '
	(
	cd repo &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	clean_checkout test-signoff-name main &&
	"$REAL_GIT" config user.name "Different User" &&
	"$REAL_GIT" config user.email "different@example.com" &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick --signoff "$PICK" &&
	"$REAL_GIT" log -1 --format=%B | grep "Signed-off-by: Different User <different@example.com>" &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com"
	)
'

test_expect_success 'cherry-pick: original commit message body preserved' '
	(
	cd repo &&
	clean_checkout test-body feature~2 &&
	"$REAL_GIT" commit --amend -m "subject line

body paragraph here" &&
	PICK=$("$REAL_GIT" rev-parse HEAD) &&
	clean_checkout test-body-pick main &&
	"$GUST_BIN" cherry-pick "$PICK" &&
	"$REAL_GIT" log -1 --format=%B | grep "body paragraph here"
	)
'

test_expect_success 'cherry-pick: tree matches git cherry-pick result' '
	(
	cd repo &&
	clean_checkout test-tree-grit main &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick "$PICK" &&
	GRIT_TREE=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	clean_checkout test-tree-git main &&
	"$REAL_GIT" cherry-pick "$PICK" &&
	GIT_TREE=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	test "$GRIT_TREE" = "$GIT_TREE"
	)
'

test_expect_success 'cherry-pick -s -x: signoff and cherry-picked-from both present' '
	(
	cd repo &&
	clean_checkout test-sx main &&
	PICK=$("$REAL_GIT" rev-parse feature~2) &&
	"$GUST_BIN" cherry-pick -s -x "$PICK" &&
	"$REAL_GIT" log -1 --format=%B | grep "Signed-off-by:" &&
	"$REAL_GIT" log -1 --format=%B | grep "(cherry picked from commit"
	)
'

test_done
