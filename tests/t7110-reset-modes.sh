#!/bin/sh
# Tests for reset --soft, --mixed, --hard modes.

test_description='reset --soft, --mixed, --hard'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ──────────────────────────────────────────────────────────────

test_expect_success 'setup repo with two commits' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "first" >file.txt &&
	grit add file.txt &&
	grit commit -m "c1" &&
	grit rev-parse HEAD >../c1_oid &&
	echo "second" >file2.txt &&
	grit add file2.txt &&
	grit commit -m "c2" &&
	grit rev-parse HEAD >../c2_oid
	)
'

# ── reset --soft ───────────────────────────────────────────────────────

test_expect_success 'reset --soft moves HEAD but keeps index and worktree' '
	(
	cd repo &&
	c1=$(cat ../c1_oid) &&
	grit reset --soft "$c1" &&
	# HEAD should point to c1
	head_oid=$(grit rev-parse HEAD) &&
	test "$head_oid" = "$c1" &&
	# file2.txt should still exist in worktree
	test -f file2.txt &&
	# file2.txt should be staged (in index)
	grit ls-files >indexed &&
	grep "file2.txt" indexed
	)
'

test_expect_success 'after soft reset, status shows staged changes' '
	(
	cd repo &&
	grit status >out &&
	grep "new file:" out || grep "Changes to be committed" out
	)
'

test_expect_success 'recommit after soft reset' '
	(
	cd repo &&
	grit commit -m "c2-again" &&
	grit rev-parse HEAD >../c2b_oid
	)
'

# ── reset --mixed (default) ───────────────────────────────────────────

test_expect_success 'reset --mixed moves HEAD and unstages but keeps worktree' '
	(
	cd repo &&
	c1=$(cat ../c1_oid) &&
	grit reset --mixed "$c1" &&
	head_oid=$(grit rev-parse HEAD) &&
	test "$head_oid" = "$c1" &&
	# file2.txt still in worktree
	test -f file2.txt &&
	# but NOT in the index (unstaged)
	grit ls-files >indexed &&
	! grep "file2.txt" indexed
	)
'

test_expect_success 'after mixed reset, status shows untracked file' '
	(
	cd repo &&
	grit status >out &&
	grep "file2.txt" out
	)
'

test_expect_success 'restage and recommit after mixed reset' '
	(
	cd repo &&
	grit add file2.txt &&
	grit commit -m "c2-mixed" &&
	grit rev-parse HEAD >../c2c_oid
	)
'

# ── reset --hard ───────────────────────────────────────────────────────

test_expect_success 'reset --hard moves HEAD, resets index and worktree' '
	(
	cd repo &&
	c1=$(cat ../c1_oid) &&
	grit reset --hard "$c1" &&
	head_oid=$(grit rev-parse HEAD) &&
	test "$head_oid" = "$c1" &&
	# file2.txt should be gone from worktree
	! test -f file2.txt
	)
'

test_expect_success 'after hard reset, working tree is clean (no staged changes)' '
	(
	cd repo &&
	grit status >out &&
	# No staged or modified tracked files (untracked leftovers are OK)
	! grep "Changes to be committed" out &&
	! grep "Changes not staged" out
	)
'

test_expect_success 'only file.txt remains after hard reset' '
	(
	cd repo &&
	grit ls-files >indexed &&
	test_line_count = 1 indexed &&
	grep "file.txt" indexed
	)
'

# ── reset without mode flag defaults to --mixed ───────────────────────

test_expect_success 'setup for default-mode test' '
	(
	cd repo &&
	echo "extra" >extra.txt &&
	grit add extra.txt &&
	grit commit -m "add extra" &&
	grit rev-parse HEAD >../c_extra_oid
	)
'

test_expect_success 'reset (no flag) defaults to mixed behaviour' '
	(
	cd repo &&
	c1=$(cat ../c1_oid) &&
	grit reset "$c1" &&
	head_oid=$(grit rev-parse HEAD) &&
	test "$head_oid" = "$c1" &&
	# extra.txt in worktree but not index
	test -f extra.txt &&
	grit ls-files >indexed &&
	! grep "extra.txt" indexed
	)
'

# ── reset to HEAD (no-op-ish) ─────────────────────────────────────────

test_expect_success 'setup clean state for HEAD reset' '
	(
	cd repo &&
	grit add extra.txt &&
	grit commit -m "re-add extra"
	)
'

test_expect_success 'reset --soft HEAD is a no-op' '
	(
	cd repo &&
	before=$(grit rev-parse HEAD) &&
	grit reset --soft HEAD &&
	after=$(grit rev-parse HEAD) &&
	test "$before" = "$after"
	)
'

test_expect_success 'reset --hard HEAD restores modified file' '
	(
	cd repo &&
	grit rev-parse HEAD >../pre_reset_head &&
	echo "dirty" >>file.txt &&
	grit reset --hard HEAD &&
	# HEAD unchanged
	post=$(grit rev-parse HEAD) &&
	pre=$(cat ../pre_reset_head) &&
	test "$post" = "$pre" &&
	# file.txt should match what is in index (no diff)
	grit diff --name-only >diff_out &&
	! grep "file.txt" diff_out
	)
'

# ── reset with -q/--quiet ─────────────────────────────────────────────

test_expect_success 'reset --hard -q suppresses output' '
	(
	cd repo &&
	echo "noise" >>file.txt &&
	grit reset --hard -q HEAD >out 2>&1 &&
	test_line_count = 0 out
	)
'

# ── reset specific paths ──────────────────────────────────────────────

test_expect_success 'setup for path reset' '
	(
	cd repo &&
	echo "modified" >file.txt &&
	echo "also modified" >extra.txt &&
	grit add file.txt extra.txt
	)
'

test_expect_success 'reset -- file.txt unstages only file.txt' '
	(
	cd repo &&
	grit reset -- file.txt &&
	grit status >out &&
	# extra.txt should still be staged
	grep "extra.txt" out
	)
'

# ── reset across multiple commits ─────────────────────────────────────

test_expect_success 'setup three commits' '
	(
	grit init multi &&
	cd multi &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo a >a.txt && grit add a.txt && grit commit -m "ma" &&
	grit rev-parse HEAD >../ma_oid &&
	echo b >b.txt && grit add b.txt && grit commit -m "mb" &&
	echo c >c.txt && grit add c.txt && grit commit -m "mc"
	)
'

test_expect_success 'hard reset back two commits removes both files' '
	(
	cd multi &&
	ma=$(cat ../ma_oid) &&
	grit reset --hard "$ma" &&
	! test -f b.txt &&
	! test -f c.txt &&
	test -f a.txt
	)
'

test_done
