#!/bin/sh
# Tests for grit reset with --soft, --mixed (default), --hard, -q,
# path-based reset, and various reset scenarios.

test_description='grit reset --soft/--mixed/--hard with paths and options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with multiple commits' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@example.com" &&
	git config user.name "Test User" &&
	echo "v1" >file.txt &&
	echo "a1" >a.txt &&
	grit add . &&
	grit commit -m "commit 1" &&
	echo "v2" >file.txt &&
	echo "a2" >a.txt &&
	grit add . &&
	grit commit -m "commit 2" &&
	echo "v3" >file.txt &&
	echo "a3" >a.txt &&
	grit add . &&
	grit commit -m "commit 3" &&
	C1=$(grit rev-parse HEAD~2) &&
	C2=$(grit rev-parse HEAD~1) &&
	C3=$(grit rev-parse HEAD) &&
	echo "$C1" >"$TRASH_DIRECTORY/c1_oid" &&
	echo "$C2" >"$TRASH_DIRECTORY/c2_oid" &&
	echo "$C3" >"$TRASH_DIRECTORY/c3_oid"
	)
'

# --- --mixed (default) ---

test_expect_success 'reset --mixed moves HEAD and unstages' '
	(
	cd repo &&
	C2=$(cat "$TRASH_DIRECTORY/c2_oid") &&
	grit reset --mixed "$C2" &&
	CURRENT=$(grit rev-parse HEAD) &&
	test "$CURRENT" = "$C2" &&
	test "$(cat file.txt)" = "v3" &&
	grit diff --cached --name-only >staged &&
	test_must_be_empty staged
	)
'

test_expect_success 'reset without flags defaults to --mixed' '
	(
	cd repo &&
	grit add file.txt a.txt &&
	grit commit -m "re-commit 3" &&
	C2=$(cat "$TRASH_DIRECTORY/c2_oid") &&
	grit reset "$C2" &&
	CURRENT=$(grit rev-parse HEAD) &&
	test "$CURRENT" = "$C2" &&
	test "$(cat file.txt)" = "v3"
	)
'

test_expect_success 'reset --mixed keeps working tree changes' '
	(
	cd repo &&
	grit add . &&
	grit commit -m "re-commit again" &&
	echo "local mod" >file.txt &&
	grit add file.txt &&
	grit reset --mixed HEAD &&
	test "$(cat file.txt)" = "local mod"
	)
'

# --- --soft ---

test_expect_success 'reset --soft moves HEAD but keeps index and worktree' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	echo "soft_test" >file.txt &&
	grit add file.txt &&
	grit commit -m "soft test commit" &&
	PREV=$(grit rev-parse HEAD~1) &&
	grit reset --soft "$PREV" &&
	CURRENT=$(grit rev-parse HEAD) &&
	test "$CURRENT" = "$PREV" &&
	test "$(cat file.txt)" = "soft_test" &&
	grit diff --cached --name-only >staged &&
	grep "file.txt" staged
	)
'

test_expect_success 'reset --soft preserves staged changes' '
	(
	cd repo &&
	grit commit -m "commit staged" &&
	echo "extra" >extra.txt &&
	grit add extra.txt &&
	echo "extra2" >extra2.txt &&
	grit add extra2.txt &&
	grit commit -m "with extras" &&
	PREV=$(grit rev-parse HEAD~1) &&
	grit reset --soft "$PREV" &&
	grit diff --cached --name-only >staged &&
	grep "extra" staged &&
	grit commit -m "re-commit with extra" &&
	rm -f extra.txt extra2.txt
	)
'

test_expect_success 'reset --soft HEAD is a no-op' '
	(
	cd repo &&
	BEFORE=$(grit rev-parse HEAD) &&
	grit reset --soft HEAD &&
	AFTER=$(grit rev-parse HEAD) &&
	test "$BEFORE" = "$AFTER"
	)
'

# --- --hard ---

test_expect_success 'reset --hard resets everything' '
	(
	cd repo &&
	echo "throwaway" >file.txt &&
	grit add file.txt &&
	echo "more throwaway" >file.txt &&
	PREV=$(grit rev-parse HEAD) &&
	grit reset --hard HEAD &&
	test "$(grit rev-parse HEAD)" = "$PREV" &&
	grit diff --name-only >worktree_diff &&
	test_must_be_empty worktree_diff &&
	grit diff --cached --name-only >staged_diff &&
	test_must_be_empty staged_diff
	)
'

test_expect_success 'reset --hard to previous commit' '
	(
	cd repo &&
	echo "new content" >file.txt &&
	grit add file.txt &&
	grit commit -m "will reset" &&
	PREV=$(grit rev-parse HEAD~1) &&
	grit reset --hard "$PREV" &&
	test "$(grit rev-parse HEAD)" = "$PREV"
	)
'

test_expect_success 'reset --hard discards working tree changes' '
	(
	cd repo &&
	echo "discard me" >file.txt &&
	echo "discard too" >a.txt &&
	grit reset --hard HEAD &&
	grit diff --name-only >diff_out &&
	test_must_be_empty diff_out
	)
'

test_expect_success 'reset --hard restores deleted files' '
	(
	cd repo &&
	rm file.txt &&
	! test -f file.txt &&
	grit reset --hard HEAD &&
	test -f file.txt
	)
'

# --- path-based reset ---

test_expect_success 'reset HEAD -- path unstages specific file' '
	(
	cd repo &&
	echo "staged1" >file.txt &&
	echo "staged2" >a.txt &&
	grit add file.txt a.txt &&
	grit reset HEAD -- file.txt &&
	grit diff --cached --name-only >staged &&
	! grep "file.txt" staged &&
	grep "a.txt" staged &&
	grit reset HEAD -- a.txt
	)
'

test_expect_success 'reset HEAD -- multiple paths' '
	(
	cd repo &&
	echo "s1" >file.txt &&
	echo "s2" >a.txt &&
	grit add file.txt a.txt &&
	grit reset HEAD -- file.txt a.txt &&
	grit diff --cached --name-only >staged &&
	test_must_be_empty staged
	)
'

test_expect_success 'reset path does not move HEAD' '
	(
	cd repo &&
	echo "new" >file.txt &&
	grit add file.txt &&
	HEAD_BEFORE=$(grit rev-parse HEAD) &&
	C2=$(cat "$TRASH_DIRECTORY/c2_oid") &&
	grit reset "$C2" -- file.txt &&
	HEAD_AFTER=$(grit rev-parse HEAD) &&
	test "$HEAD_BEFORE" = "$HEAD_AFTER"
	)
'

# --- quiet mode ---

test_expect_success 'reset -q suppresses output' '
	(
	cd repo &&
	echo "q" >file.txt &&
	grit add file.txt &&
	grit reset -q HEAD >out 2>&1 &&
	test_must_be_empty out
	)
'

test_expect_success 'reset --quiet suppresses output' '
	(
	cd repo &&
	echo "q2" >file.txt &&
	grit add file.txt &&
	grit reset --quiet HEAD >out 2>&1 &&
	test_must_be_empty out
	)
'

# --- reset to specific commits ---

test_expect_success 'reset --hard to first commit' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/c1_oid") &&
	grit reset --hard "$C1" &&
	test "$(grit rev-parse HEAD)" = "$C1"
	)
'

test_expect_success 'reset --hard back to latest' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/c3_oid") &&
	grit reset --hard "$C3" &&
	test -f file.txt &&
	test "$(cat file.txt)" = "v3"
	)
'

# --- reset with new files ---

test_expect_success 'reset HEAD does not remove untracked files' '
	(
	cd repo &&
	echo "untracked" >untracked.txt &&
	grit reset --hard HEAD &&
	test -f untracked.txt &&
	rm -f untracked.txt
	)
'

test_expect_success 'reset --mixed on staged new file unstages it' '
	(
	cd repo &&
	echo "brandnew" >brandnew.txt &&
	grit add brandnew.txt &&
	grit reset HEAD -- brandnew.txt &&
	grit ls-files >ls_out &&
	! grep "brandnew.txt" ls_out &&
	test -f brandnew.txt &&
	rm -f brandnew.txt
	)
'

test_expect_success 'reset --soft then commit squashes' '
	(
	cd repo &&
	echo "sq1" >sq.txt &&
	grit add sq.txt &&
	grit commit -m "sq commit 1" &&
	echo "sq2" >sq.txt &&
	grit add sq.txt &&
	grit commit -m "sq commit 2" &&
	SQUASH_BASE=$(grit rev-parse HEAD~2) &&
	grit reset --soft "$SQUASH_BASE" &&
	grit commit -m "squashed" &&
	grit log --oneline >log_out &&
	grep "squashed" log_out
	)
'

test_expect_success 'reset --hard removes staged and working tree changes' '
	(
	cd repo &&
	echo "stage" >file.txt &&
	grit add file.txt &&
	echo "worktree" >file.txt &&
	grit reset --hard HEAD &&
	grit diff --name-only >wt_diff &&
	grit diff --cached --name-only >idx_diff &&
	test_must_be_empty wt_diff &&
	test_must_be_empty idx_diff
	)
'

test_expect_success 'reset to same HEAD is a no-op for --mixed' '
	(
	cd repo &&
	BEFORE=$(grit rev-parse HEAD) &&
	grit reset --mixed HEAD &&
	AFTER=$(grit rev-parse HEAD) &&
	test "$BEFORE" = "$AFTER"
	)
'

test_expect_success 'reset --hard HEAD is idempotent' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	grit reset --hard HEAD &&
	grit diff --name-only >out &&
	test_must_be_empty out
	)
'

test_expect_success 'reset with tag reference' '
	(
	cd repo &&
	grit tag reset-point HEAD &&
	echo "after tag" >file.txt &&
	grit add file.txt &&
	grit commit -m "after tag commit" &&
	grit reset --hard reset-point &&
	test "$(grit rev-parse HEAD)" = "$(grit rev-parse reset-point)"
	)
'

test_done
