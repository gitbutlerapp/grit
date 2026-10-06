#!/bin/sh
# Tests for grit cherry-pick: message handling, -x, --signoff,
# -n/--no-commit, multiple picks, and conflict scenarios.

test_description='grit cherry-pick message, -x, --signoff, -n, conflicts'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with diverging branches' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@example.com" &&
	git config user.name "Test User" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "base" >file.txt &&
	echo "shared" >shared.txt &&
	grit add . &&
	grit commit -m "initial" &&
	INITIAL=$(grit rev-parse HEAD) &&
	grit branch side &&
	grit branch side2 &&
	grit branch conflict-branch &&
	echo "main-line1" >main-only.txt &&
	grit add main-only.txt &&
	grit commit -m "main: add main-only" &&
	MAIN_C1=$(grit rev-parse HEAD) &&
	echo "main-line2" >>file.txt &&
	grit add file.txt &&
	grit commit -m "main: update file" &&
	grit switch side &&
	echo "side-file" >side.txt &&
	grit add side.txt &&
	grit commit -m "side: add side.txt" &&
	SIDE_C1=$(grit rev-parse HEAD) &&
	echo "side-file2" >side2.txt &&
	grit add side2.txt &&
	grit commit -m "side: add side2.txt" &&
	SIDE_C2=$(grit rev-parse HEAD) &&
	echo "side-file3" >side3.txt &&
	grit add side3.txt &&
	grit commit -m "side: add side3.txt" &&
	SIDE_C3=$(grit rev-parse HEAD) &&
	grit switch side2 &&
	echo "s2-content" >s2.txt &&
	grit add s2.txt &&
	grit commit -m "side2: add s2.txt" &&
	SIDE2_C1=$(grit rev-parse HEAD) &&
	grit switch conflict-branch &&
	echo "conflict-line" >>file.txt &&
	grit add file.txt &&
	grit commit -m "conflict: modify file.txt" &&
	CONFLICT_C1=$(grit rev-parse HEAD) &&
	grit switch main &&
	echo "$INITIAL" >"$TRASH_DIRECTORY/oid_initial" &&
	echo "$MAIN_C1" >"$TRASH_DIRECTORY/oid_main_c1" &&
	echo "$SIDE_C1" >"$TRASH_DIRECTORY/oid_side_c1" &&
	echo "$SIDE_C2" >"$TRASH_DIRECTORY/oid_side_c2" &&
	echo "$SIDE_C3" >"$TRASH_DIRECTORY/oid_side_c3" &&
	echo "$SIDE2_C1" >"$TRASH_DIRECTORY/oid_side2_c1" &&
	echo "$CONFLICT_C1" >"$TRASH_DIRECTORY/oid_conflict_c1"
	)
'

# --- basic cherry-pick ---

test_expect_success 'cherry-pick applies commit from side branch' '
	(
	cd repo &&
	SIDE_C1=$(cat "$TRASH_DIRECTORY/oid_side_c1") &&
	grit cherry-pick "$SIDE_C1" &&
	test -f side.txt &&
	test "$(cat side.txt)" = "side-file"
	)
'

test_expect_success 'cherry-pick preserves original commit message' '
	(
	cd repo &&
	grit cat-file -p HEAD >commit_msg &&
	grep "side: add side.txt" commit_msg
	)
'

test_expect_success 'cherry-pick preserves original author' '
	(
	cd repo &&
	grit cat-file -p HEAD >commit_info &&
	grep "author Test User" commit_info
	)
'

test_expect_success 'cherry-picked commit is a new commit (different OID)' '
	(
	cd repo &&
	SIDE_C1=$(cat "$TRASH_DIRECTORY/oid_side_c1") &&
	CURRENT=$(grit rev-parse HEAD) &&
	test "$CURRENT" != "$SIDE_C1"
	)
'

# --- cherry-pick -x ---

test_expect_success 'cherry-pick -x appends cherry-picked-from line' '
	(
	cd repo &&
	SIDE_C2=$(cat "$TRASH_DIRECTORY/oid_side_c2") &&
	grit cherry-pick -x "$SIDE_C2" &&
	grit cat-file -p HEAD >commit_msg &&
	grep "cherry picked from commit" commit_msg
	)
'

test_expect_success 'cherry-pick -x references original commit hash' '
	(
	cd repo &&
	SIDE_C2=$(cat "$TRASH_DIRECTORY/oid_side_c2") &&
	grit cat-file -p HEAD >commit_msg &&
	grep "$SIDE_C2" commit_msg
	)
'

test_expect_success 'cherry-pick -x preserves original message too' '
	(
	cd repo &&
	grit cat-file -p HEAD >commit_msg &&
	grep "side: add side2.txt" commit_msg
	)
'

# --- cherry-pick --signoff ---

test_expect_success 'cherry-pick --signoff adds Signed-off-by line' '
	(
	cd repo &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	SIDE_C3=$(cat "$TRASH_DIRECTORY/oid_side_c3") &&
	grit cherry-pick --signoff "$SIDE_C3" &&
	grit cat-file -p HEAD >commit_msg &&
	grep "Signed-off-by:" commit_msg
	)
'

test_expect_success 'Signed-off-by contains committer info' '
	(
	cd repo &&
	grit cat-file -p HEAD >commit_msg &&
	grep "Signed-off-by: Test User" commit_msg
	)
'

# --- cherry-pick -n / --no-commit ---

test_expect_success 'cherry-pick -n stages changes but does not commit' '
	(
	cd repo &&
	SIDE2_C1=$(cat "$TRASH_DIRECTORY/oid_side2_c1") &&
	BEFORE=$(grit rev-parse HEAD) &&
	grit cherry-pick -n "$SIDE2_C1" &&
	AFTER=$(grit rev-parse HEAD) &&
	test "$BEFORE" = "$AFTER" &&
	test -f s2.txt
	)
'

test_expect_success 'cherry-pick -n leaves changes staged' '
	(
	cd repo &&
	grit diff --cached --name-only >cached &&
	grep "s2.txt" cached
	)
'

test_expect_success 'can commit after cherry-pick -n with custom message' '
	(
	cd repo &&
	grit commit -m "custom message for s2" &&
	grit cat-file -p HEAD >commit_msg &&
	grep "custom message for s2" commit_msg
	)
'

test_expect_success 'setup: create side3 commit for no-commit test' '
	(
	cd repo &&
	grit switch side &&
	echo "nc-content" >nc.txt &&
	grit add nc.txt &&
	grit commit -m "side: nc file" &&
	NC_OID=$(grit rev-parse HEAD) &&
	echo "$NC_OID" >"$TRASH_DIRECTORY/oid_nc" &&
	grit switch main
	)
'

test_expect_success 'cherry-pick --no-commit is same as -n' '
	(
	cd repo &&
	NC_OID=$(cat "$TRASH_DIRECTORY/oid_nc") &&
	BEFORE=$(grit rev-parse HEAD) &&
	grit cherry-pick --no-commit "$NC_OID" &&
	AFTER=$(grit rev-parse HEAD) &&
	test "$BEFORE" = "$AFTER" &&
	grit diff --cached --name-only >cached &&
	grep "nc.txt" cached &&
	grit reset --hard HEAD
	)
'

# --- cherry-pick -x combined with --signoff ---

test_expect_success 'setup for combination tests' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	grit switch side &&
	echo "combo" >combo.txt &&
	grit add combo.txt &&
	grit commit -m "side: combo file" &&
	COMBO=$(grit rev-parse HEAD) &&
	echo "$COMBO" >"$TRASH_DIRECTORY/oid_combo" &&
	grit switch main
	)
'

test_expect_success 'cherry-pick -x --signoff adds both annotations' '
	(
	cd repo &&
	COMBO=$(cat "$TRASH_DIRECTORY/oid_combo") &&
	grit cherry-pick -x --signoff "$COMBO" &&
	grit cat-file -p HEAD >commit_msg &&
	grep "cherry picked from commit" commit_msg &&
	grep "Signed-off-by:" commit_msg
	)
'

# --- cherry-pick multiple commits ---

test_expect_success 'setup side branch with sequential commits for multi-pick' '
	(
	cd repo &&
	grit switch side &&
	echo "mp1" >mp1.txt &&
	grit add mp1.txt &&
	grit commit -m "multi-pick 1" &&
	MP1=$(grit rev-parse HEAD) &&
	echo "mp2" >mp2.txt &&
	grit add mp2.txt &&
	grit commit -m "multi-pick 2" &&
	MP2=$(grit rev-parse HEAD) &&
	echo "$MP1" >"$TRASH_DIRECTORY/oid_mp1" &&
	echo "$MP2" >"$TRASH_DIRECTORY/oid_mp2" &&
	grit switch main
	)
'

test_expect_success 'cherry-pick two commits sequentially' '
	(
	cd repo &&
	MP1=$(cat "$TRASH_DIRECTORY/oid_mp1") &&
	MP2=$(cat "$TRASH_DIRECTORY/oid_mp2") &&
	grit cherry-pick "$MP1" &&
	grit cherry-pick "$MP2" &&
	test -f mp1.txt &&
	test -f mp2.txt
	)
'

test_expect_success 'both cherry-picked commits have correct messages' '
	(
	cd repo &&
	grit cat-file -p HEAD >msg2 &&
	grep "multi-pick 2" msg2 &&
	PARENT=$(grit rev-parse HEAD^) &&
	grit cat-file -p "$PARENT" >msg1 &&
	grep "multi-pick 1" msg1
	)
'

# --- cherry-pick conflict ---

test_expect_success 'cherry-pick conflicting commit fails' '
	(
	cd repo &&
	CONFLICT_C1=$(cat "$TRASH_DIRECTORY/oid_conflict_c1") &&
	test_must_fail grit cherry-pick "$CONFLICT_C1" 2>err
	)
'

test_expect_success 'abort cherry-pick after conflict' '
	(
	cd repo &&
	grit cherry-pick --abort 2>/dev/null ||
	grit reset --hard HEAD
	)
'

# --- cherry-pick with empty result ---

test_expect_success 'setup: create commit already applied' '
	(
	cd repo &&
	grit switch side &&
	echo "already" >already.txt &&
	grit add already.txt &&
	grit commit -m "add already" &&
	ALREADY=$(grit rev-parse HEAD) &&
	echo "$ALREADY" >"$TRASH_DIRECTORY/oid_already" &&
	grit switch main &&
	echo "already" >already.txt &&
	grit add already.txt &&
	grit commit -m "add already on main"
	)
'

test_expect_success 'cherry-pick of already-applied commit results in empty' '
	(
	cd repo &&
	ALREADY=$(cat "$TRASH_DIRECTORY/oid_already") &&
	test_must_fail grit cherry-pick "$ALREADY" 2>err
	)
'

test_expect_success 'cleanup after empty cherry-pick' '
	(
	cd repo &&
	grit cherry-pick --abort 2>/dev/null ||
	grit reset --hard HEAD
	)
'

# --- cherry-pick onto different branch ---

test_expect_success 'cherry-pick works on a freshly created branch' '
	(
	cd repo &&
	INITIAL=$(cat "$TRASH_DIRECTORY/oid_initial") &&
	grit switch -c pick-target "$INITIAL" &&
	SIDE_C1=$(cat "$TRASH_DIRECTORY/oid_side_c1") &&
	grit cherry-pick "$SIDE_C1" &&
	test -f side.txt
	)
'

test_expect_success 'cherry-pick invalid ref fails' '
	(
	cd repo &&
	test_must_fail grit cherry-pick invalid-ref 2>err
	)
'

test_expect_success 'cherry-pick with no args fails' '
	(
	cd repo &&
	test_must_fail grit cherry-pick 2>err
	)
'

# --- cherry-pick -n then commit with --signoff ---

test_expect_success 'cherry-pick -n then manual commit preserves staged changes' '
	(
	cd repo &&
	grit switch main &&
	NC_OID=$(cat "$TRASH_DIRECTORY/oid_nc") &&
	grit cherry-pick -n "$NC_OID" &&
	test -f nc.txt &&
	grit commit -m "manually committed nc" &&
	grit cat-file -p HEAD >msg &&
	grep "manually committed nc" msg &&
	test -f nc.txt
	)
'

test_expect_success 'cherry-pick -x -n does not create commit but stages' '
	(
	cd repo &&
	grit switch side &&
	echo "xn-content" >xn.txt &&
	grit add xn.txt &&
	grit commit -m "side: xn file" &&
	XN_OID=$(grit rev-parse HEAD) &&
	grit switch main &&
	BEFORE=$(grit rev-parse HEAD) &&
	grit cherry-pick -x -n "$XN_OID" &&
	AFTER=$(grit rev-parse HEAD) &&
	test "$BEFORE" = "$AFTER" &&
	test -f xn.txt &&
	grit reset --hard HEAD
	)
'

test_done
