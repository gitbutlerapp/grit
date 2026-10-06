#!/bin/sh
# Tests for grit reset with --hard, --soft, --mixed modes,
# path-based resets, and various edge cases.

test_description='grit reset --hard, --soft, --mixed, and path resets'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with three commits' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@example.com" &&
	git config user.name "Test User" &&
	echo "v1" >file.txt &&
	echo "a1" >a.txt &&
	echo "b1" >b.txt &&
	mkdir -p sub &&
	echo "s1" >sub/s.txt &&
	grit add . &&
	grit commit -m "commit1" &&
	C1=$(grit rev-parse HEAD) &&
	echo "v2" >file.txt &&
	echo "a2" >a.txt &&
	echo "b2" >b.txt &&
	echo "s2" >sub/s.txt &&
	grit add . &&
	grit commit -m "commit2" &&
	C2=$(grit rev-parse HEAD) &&
	echo "v3" >file.txt &&
	echo "a3" >a.txt &&
	echo "b3" >b.txt &&
	echo "s3" >sub/s.txt &&
	grit add . &&
	grit commit -m "commit3" &&
	C3=$(grit rev-parse HEAD) &&
	echo "$C1" >"$TRASH_DIRECTORY/oid_c1" &&
	echo "$C2" >"$TRASH_DIRECTORY/oid_c2" &&
	echo "$C3" >"$TRASH_DIRECTORY/oid_c3"
	)
'

# --- --soft reset ---

test_expect_success 'reset --soft moves HEAD but keeps index and worktree' '
	(
	cd repo &&
	C2=$(cat "$TRASH_DIRECTORY/oid_c2") &&
	grit reset --soft "$C2" &&
	test "$(grit rev-parse HEAD)" = "$C2" &&
	test "$(cat file.txt)" = "v3" &&
	test "$(cat a.txt)" = "a3"
	)
'

test_expect_success 'after --soft reset, diff --cached shows staged changes' '
	(
	cd repo &&
	grit diff --cached --name-only >cached &&
	grep "file.txt" cached &&
	grep "a.txt" cached
	)
'

test_expect_success 'reset --soft back to original' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_c3") &&
	grit reset --soft "$C3" &&
	test "$(grit rev-parse HEAD)" = "$C3"
	)
'

test_expect_success 'reset --soft to grandparent' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_c1") &&
	grit reset --soft "$C1" &&
	test "$(grit rev-parse HEAD)" = "$C1" &&
	test "$(cat file.txt)" = "v3" &&
	grit diff --cached --name-only >cached &&
	grep "file.txt" cached
	)
'

test_expect_success 'restore to C3 for next tests' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_c3") &&
	grit reset --hard "$C3"
	)
'

# --- --mixed reset (default) ---

test_expect_success 'reset --mixed moves HEAD and resets index but keeps worktree' '
	(
	cd repo &&
	C2=$(cat "$TRASH_DIRECTORY/oid_c2") &&
	grit reset --mixed "$C2" &&
	test "$(grit rev-parse HEAD)" = "$C2" &&
	test "$(cat file.txt)" = "v3" &&
	grit diff --name-only >unstaged &&
	grep "file.txt" unstaged
	)
'

test_expect_success 'reset without mode flag defaults to --mixed' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_c3") &&
	grit reset "$C3" &&
	C1=$(cat "$TRASH_DIRECTORY/oid_c1") &&
	grit reset "$C1" &&
	test "$(grit rev-parse HEAD)" = "$C1" &&
	test "$(cat file.txt)" = "v3"
	)
'

test_expect_success 'restore to C3 for hard tests' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_c3") &&
	grit reset --hard "$C3"
	)
'

# --- --hard reset ---

test_expect_success 'reset --hard moves HEAD, resets index and worktree' '
	(
	cd repo &&
	C2=$(cat "$TRASH_DIRECTORY/oid_c2") &&
	grit reset --hard "$C2" &&
	test "$(grit rev-parse HEAD)" = "$C2" &&
	test "$(cat file.txt)" = "v2" &&
	test "$(cat a.txt)" = "a2"
	)
'

test_expect_success 'reset --hard leaves no unstaged changes' '
	(
	cd repo &&
	grit diff --name-only >unstaged &&
	test ! -s unstaged
	)
'

test_expect_success 'reset --hard leaves no staged changes' '
	(
	cd repo &&
	grit diff --cached --name-only >cached &&
	test ! -s cached
	)
'

test_expect_success 'reset --hard to grandparent' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_c1") &&
	grit reset --hard "$C1" &&
	test "$(cat file.txt)" = "v1" &&
	test "$(cat a.txt)" = "a1"
	)
'

test_expect_success 'reset --hard forward to C3' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_c3") &&
	grit reset --hard "$C3" &&
	test "$(cat file.txt)" = "v3"
	)
'

# --- path-based reset ---

test_expect_success 'reset HEAD -- path unstages a file' '
	(
	cd repo &&
	echo "changed" >file.txt &&
	grit add file.txt &&
	grit reset HEAD -- file.txt &&
	grit diff --cached --name-only >cached &&
	! grep "file.txt" cached &&
	test "$(cat file.txt)" = "changed"
	)
'

test_expect_success 'reset HEAD -- with multiple paths' '
	(
	cd repo &&
	echo "ca" >a.txt &&
	echo "cb" >b.txt &&
	grit add a.txt b.txt &&
	grit reset HEAD -- a.txt b.txt &&
	grit diff --cached --name-only >cached &&
	! grep "a.txt" cached &&
	! grep "b.txt" cached
	)
'

test_expect_success 'reset path does not move HEAD' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_c3") &&
	echo "new" >file.txt &&
	grit add file.txt &&
	grit reset HEAD -- file.txt &&
	test "$(grit rev-parse HEAD)" = "$C3"
	)
'

test_expect_success 'reset parent -- path puts old version in index' '
	(
	cd repo &&
	grit restore . &&
	C2=$(cat "$TRASH_DIRECTORY/oid_c2") &&
	grit reset "$C2" -- file.txt &&
	grit diff --cached --name-only >cached &&
	grep "file.txt" cached
	)
'

test_expect_success 'reset HEAD -- path on nested file' '
	(
	cd repo &&
	grit reset HEAD -- file.txt &&
	echo "new-sub" >sub/s.txt &&
	grit add sub/s.txt &&
	grit reset HEAD -- sub/s.txt &&
	grit diff --cached --name-only >cached &&
	! grep "sub/s.txt" cached
	)
'

# --- reset with dirty worktree ---

test_expect_success 'reset --hard discards uncommitted changes' '
	(
	cd repo &&
	grit restore . &&
	echo "dirty" >file.txt &&
	echo "dirty-a" >a.txt &&
	grit add file.txt a.txt &&
	echo "extra-dirty" >file.txt &&
	C3=$(cat "$TRASH_DIRECTORY/oid_c3") &&
	grit reset --hard "$C3" &&
	test "$(cat file.txt)" = "v3" &&
	test "$(cat a.txt)" = "a3"
	)
'

test_expect_success 'reset --soft preserves dirty worktree' '
	(
	cd repo &&
	echo "dirty-soft" >file.txt &&
	C2=$(cat "$TRASH_DIRECTORY/oid_c2") &&
	grit reset --soft "$C2" &&
	test "$(cat file.txt)" = "dirty-soft"
	)
'

test_expect_success 'restore to C3' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_c3") &&
	grit reset --hard "$C3"
	)
'

# --- reset to HEAD (re-baseline) ---

test_expect_success 'reset HEAD with no mode unstages all' '
	(
	cd repo &&
	echo "x" >file.txt &&
	echo "y" >a.txt &&
	grit add file.txt a.txt &&
	grit reset HEAD &&
	grit diff --cached --name-only >cached &&
	test ! -s cached
	)
'

test_expect_success 'reset --hard HEAD restores all to committed state' '
	(
	cd repo &&
	echo "messy" >file.txt &&
	grit reset --hard HEAD &&
	test "$(cat file.txt)" = "v3"
	)
'

# --- quiet mode ---

test_expect_success 'reset -q suppresses output' '
	(
	cd repo &&
	echo "q" >file.txt &&
	grit add file.txt &&
	grit reset -q HEAD -- file.txt >out 2>&1 &&
	test ! -s out
	)
'

test_expect_success 'reset --quiet --hard suppresses output' '
	(
	cd repo &&
	C2=$(cat "$TRASH_DIRECTORY/oid_c2") &&
	grit reset --quiet --hard "$C2" >out 2>&1 &&
	test ! -s out
	)
'

# --- edge cases ---

test_expect_success 'reset to invalid ref fails' '
	(
	cd repo &&
	test_must_fail grit reset --hard invalid-ref 2>err
	)
'

test_expect_success 'reset --hard with new untracked file preserves it' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_c3") &&
	grit reset --hard "$C3" &&
	echo "untracked" >untracked.txt &&
	C2=$(cat "$TRASH_DIRECTORY/oid_c2") &&
	grit reset --hard "$C2" &&
	test -f untracked.txt
	)
'

test_expect_success 'reset path with nonexistent file fails' '
	(
	cd repo &&
	test_must_fail grit reset HEAD -- nonexistent.txt 2>err
	)
'

test_expect_success 'reset --mixed preserves new file in worktree' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_c3") &&
	grit reset --hard "$C3" &&
	echo "new-mixed" >new_file.txt &&
	grit add new_file.txt &&
	grit reset HEAD &&
	test -f new_file.txt &&
	test "$(cat new_file.txt)" = "new-mixed"
	)
'

test_expect_success 'reset --soft then commit effectively amends' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_c3") &&
	grit reset --hard "$C3" &&
	C2=$(cat "$TRASH_DIRECTORY/oid_c2") &&
	grit reset --soft "$C2" &&
	grit commit -m "amended commit3" &&
	grit cat-file -p HEAD >log_out &&
	grep "amended commit3" log_out
	)
'

test_done
