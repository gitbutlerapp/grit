#!/bin/sh
# Tests for grit cherry-pick with --abort and --continue scenarios.

test_description='grit cherry-pick: conflict handling, --abort, --continue, multi-commit picks'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with divergent branches' '
	(
	"$REAL_GIT" init -b main repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "initial" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial commit" &&
	"$REAL_GIT" branch base &&

	echo "line1 from main" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "main change 1" &&

	echo "line2 from main" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "main change 2" &&

	"$REAL_GIT" checkout base &&
	echo "line1 from branch" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "branch change 1" &&

	echo "line2 from branch" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "branch change 2" &&

	echo "no conflict content" >other.txt &&
	"$REAL_GIT" add other.txt &&
	"$REAL_GIT" commit -m "branch change 3 (no conflict)" &&

	"$REAL_GIT" checkout main
	)
'

###########################################################################
# Section 2: Basic cherry-pick
###########################################################################

test_expect_success 'cherry-pick: pick a non-conflicting commit' '
	(
	cd repo &&
	"$REAL_GIT" checkout main &&
	PICK=$("$REAL_GIT" rev-parse base) &&
	"$GUST_BIN" cherry-pick base~0 &&
	test -f other.txt
	)
'

test_expect_success 'cherry-pick: result has correct content' '
	(
	cd repo &&
	cat other.txt | grep "no conflict content"
	)
'

test_expect_success 'cherry-pick: creates new commit' '
	(
	cd repo &&
	BEFORE=$("$REAL_GIT" rev-parse HEAD~1) &&
	AFTER=$("$REAL_GIT" rev-parse HEAD) &&
	test "$BEFORE" != "$AFTER"
	)
'

test_expect_success 'cherry-pick: commit message preserved' '
	(
	cd repo &&
	"$REAL_GIT" log -1 --format=%s HEAD >msg.txt &&
	grep "branch change 3 (no conflict)" msg.txt
	)
'

###########################################################################
# Section 3: Cherry-pick with conflicts
###########################################################################

test_expect_success 'setup: clean state for conflict tests' '
	(
	"$REAL_GIT" init -b main conflict-repo &&
	cd conflict-repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "base content" >shared.txt &&
	"$REAL_GIT" add shared.txt &&
	"$REAL_GIT" commit -m "base" &&
	"$REAL_GIT" branch side &&

	echo "main version" >shared.txt &&
	"$REAL_GIT" add shared.txt &&
	"$REAL_GIT" commit -m "main edit" &&

	"$REAL_GIT" checkout side &&
	echo "side version" >shared.txt &&
	"$REAL_GIT" add shared.txt &&
	"$REAL_GIT" commit -m "side edit" &&

	"$REAL_GIT" checkout main
	)
'

test_expect_success 'cherry-pick: conflicting commit fails' '
	(
	cd conflict-repo &&
	SIDE_COMMIT=$("$REAL_GIT" rev-parse side) &&
	test_must_fail "$GUST_BIN" cherry-pick "$SIDE_COMMIT"
	)
'

test_expect_success 'cherry-pick --abort: restores pre-conflict state' '
	(
	cd conflict-repo &&
	HEAD_BEFORE=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" cherry-pick --abort &&
	HEAD_AFTER=$("$REAL_GIT" rev-parse HEAD) &&
	test "$HEAD_BEFORE" = "$HEAD_AFTER"
	)
'

test_expect_success 'cherry-pick --abort: working tree is clean after abort' '
	(
	cd conflict-repo &&
	"$REAL_GIT" diff --quiet &&
	"$REAL_GIT" diff --cached --quiet
	)
'

test_expect_success 'cherry-pick --abort: no CHERRY_PICK_HEAD after abort' '
	(
	cd conflict-repo &&
	test_path_is_missing .git/CHERRY_PICK_HEAD
	)
'

###########################################################################
# Section 4: Cherry-pick --continue
###########################################################################

test_expect_success 'setup: trigger conflict for continue test' '
	(
	cd conflict-repo &&
	SIDE_COMMIT=$("$REAL_GIT" rev-parse side) &&
	test_must_fail "$GUST_BIN" cherry-pick "$SIDE_COMMIT"
	)
'

test_expect_success 'cherry-pick: CHERRY_PICK_HEAD exists during conflict' '
	(
	cd conflict-repo &&
	test -f .git/CHERRY_PICK_HEAD
	)
'

test_expect_success 'cherry-pick --continue: resolve and continue' '
	(
	cd conflict-repo &&
	echo "resolved content" >shared.txt &&
	"$REAL_GIT" add shared.txt &&
	"$GUST_BIN" cherry-pick --continue
	)
'

test_expect_success 'cherry-pick --continue: creates commit with resolved content' '
	(
	cd conflict-repo &&
	cat shared.txt >actual.txt &&
	echo "resolved content" >expect.txt &&
	test_cmp expect.txt actual.txt
	)
'

test_expect_success 'cherry-pick --continue: CHERRY_PICK_HEAD removed' '
	(
	cd conflict-repo &&
	test_path_is_missing .git/CHERRY_PICK_HEAD
	)
'

###########################################################################
# Section 5: Multi-commit cherry-pick
###########################################################################

test_expect_success 'setup: repo for multi-pick' '
	(
	"$REAL_GIT" init -b main multi-repo &&
	cd multi-repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "base" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "base" &&
	"$REAL_GIT" branch feature &&

	"$REAL_GIT" checkout feature &&
	echo "feat1" >feat1.txt &&
	"$REAL_GIT" add feat1.txt &&
	"$REAL_GIT" commit -m "feature 1" &&
	echo "feat2" >feat2.txt &&
	"$REAL_GIT" add feat2.txt &&
	"$REAL_GIT" commit -m "feature 2" &&
	echo "feat3" >feat3.txt &&
	"$REAL_GIT" add feat3.txt &&
	"$REAL_GIT" commit -m "feature 3" &&

	"$REAL_GIT" checkout main
	)
'

test_expect_success 'cherry-pick: multiple commits in sequence' '
	(
	cd multi-repo &&
	FEAT1=$("$REAL_GIT" log --format=%H feature~2 -1) &&
	FEAT2=$("$REAL_GIT" log --format=%H feature~1 -1) &&
	FEAT3=$("$REAL_GIT" log --format=%H feature -1) &&
	"$GUST_BIN" cherry-pick "$FEAT1" "$FEAT2" "$FEAT3"
	)
'

test_expect_success 'cherry-pick: all three files exist after multi-pick' '
	(
	cd multi-repo &&
	test -f feat1.txt &&
	test -f feat2.txt &&
	test -f feat3.txt
	)
'

test_expect_success 'cherry-pick: three new commits created' '
	(
	cd multi-repo &&
	"$REAL_GIT" log --oneline HEAD~3..HEAD >log.txt &&
	test_line_count = 3 log.txt
	)
'

test_expect_success 'cherry-pick: commit messages preserved in multi-pick' '
	(
	cd multi-repo &&
	"$REAL_GIT" log --format=%s HEAD~3..HEAD >msgs.txt &&
	grep "feature 1" msgs.txt &&
	grep "feature 2" msgs.txt &&
	grep "feature 3" msgs.txt
	)
'

###########################################################################
# Section 6: Cherry-pick with --no-commit
###########################################################################

test_expect_success 'setup: repo for no-commit test' '
	(
	"$REAL_GIT" init -b main nocommit-repo &&
	cd nocommit-repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "base" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "base" &&
	"$REAL_GIT" branch side &&
	"$REAL_GIT" checkout side &&
	echo "side change" >newfile.txt &&
	"$REAL_GIT" add newfile.txt &&
	"$REAL_GIT" commit -m "side commit" &&
	"$REAL_GIT" checkout main
	)
'

test_expect_success 'cherry-pick --no-commit: stages changes without committing' '
	(
	cd nocommit-repo &&
	HEAD_BEFORE=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" cherry-pick --no-commit side &&
	HEAD_AFTER=$("$REAL_GIT" rev-parse HEAD) &&
	test "$HEAD_BEFORE" = "$HEAD_AFTER"
	)
'

test_expect_success 'cherry-pick --no-commit: file is staged' '
	(
	cd nocommit-repo &&
	"$REAL_GIT" diff --cached --name-only >staged.txt &&
	grep "newfile.txt" staged.txt
	)
'

test_expect_success 'cherry-pick --no-commit: file content correct' '
	(
	cd nocommit-repo &&
	echo "side change" >expect.txt &&
	test_cmp expect.txt newfile.txt
	)
'

###########################################################################
# Section 7: Abort edge cases
###########################################################################

test_expect_success 'cherry-pick --abort: fails when no cherry-pick in progress' '
	(
	"$REAL_GIT" init -b main abort-edge &&
	cd abort-edge &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "x" >x.txt &&
	"$REAL_GIT" add x.txt &&
	"$REAL_GIT" commit -m "init" &&
	test_must_fail "$GUST_BIN" cherry-pick --abort 2>err.txt
	)
'

test_expect_success 'cherry-pick --continue: fails when no cherry-pick in progress' '
	(
	cd abort-edge &&
	test_must_fail "$GUST_BIN" cherry-pick --continue 2>err.txt
	)
'

###########################################################################
# Section 8: Cherry-pick specific commit by SHA
###########################################################################

test_expect_success 'setup: repo for SHA-based picks' '
	(
	"$REAL_GIT" init -b main sha-repo &&
	cd sha-repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "base" >f.txt &&
	"$REAL_GIT" add f.txt &&
	"$REAL_GIT" commit -m "base" &&
	"$REAL_GIT" branch work &&
	"$REAL_GIT" checkout work &&
	echo "a" >a.txt && "$REAL_GIT" add a.txt && "$REAL_GIT" commit -m "add a" &&
	echo "b" >b.txt && "$REAL_GIT" add b.txt && "$REAL_GIT" commit -m "add b" &&
	echo "c" >c.txt && "$REAL_GIT" add c.txt && "$REAL_GIT" commit -m "add c" &&
	"$REAL_GIT" checkout main
	)
'

test_expect_success 'cherry-pick: pick single commit by full SHA' '
	(
	cd sha-repo &&
	SHA=$("$REAL_GIT" rev-parse work~1) &&
	"$GUST_BIN" cherry-pick "$SHA" &&
	test -f b.txt
	)
'

test_expect_success 'cherry-pick: pick commit by abbreviated SHA' '
	(
	cd sha-repo &&
	SHA=$("$REAL_GIT" rev-parse --short work~2) &&
	"$GUST_BIN" cherry-pick "$SHA" &&
	test -f a.txt
	)
'

test_expect_success 'cherry-pick: HEAD advances after each pick' '
	(
	cd sha-repo &&
	"$REAL_GIT" log --oneline >log.txt &&
	test_line_count -ge 3 log.txt
	)
'

###########################################################################
# Section 9: Cherry-pick with -x flag
###########################################################################

test_expect_success 'setup: repo for -x flag test' '
	(
	"$REAL_GIT" init -b main x-repo &&
	cd x-repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "base" >f.txt &&
	"$REAL_GIT" add f.txt &&
	"$REAL_GIT" commit -m "base" &&
	"$REAL_GIT" branch feat &&
	"$REAL_GIT" checkout feat &&
	echo "new" >n.txt && "$REAL_GIT" add n.txt && "$REAL_GIT" commit -m "feat commit" &&
	"$REAL_GIT" checkout main
	)
'

test_expect_success 'cherry-pick -x: appends cherry-picked-from line' '
	(
	cd x-repo &&
	FEAT_SHA=$("$REAL_GIT" rev-parse feat) &&
	"$GUST_BIN" cherry-pick -x feat &&
	"$REAL_GIT" log -1 --format=%B HEAD >body.txt &&
	grep "cherry picked from commit" body.txt
	)
'

###########################################################################
# Section 10: Cherry-pick empty and invalid args
###########################################################################

test_expect_success 'cherry-pick: invalid ref fails' '
	(
	cd x-repo &&
	test_must_fail "$GUST_BIN" cherry-pick nonexistent-ref 2>err.txt
	)
'

test_expect_success 'cherry-pick: no arguments fails' '
	(
	cd x-repo &&
	test_must_fail "$GUST_BIN" cherry-pick 2>err.txt
	)
'

test_done
