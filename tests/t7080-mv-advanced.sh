#!/bin/sh
# Advanced tests for 'grit mv': subdirs, directories, -k, -n, -f, overwrites.

test_description='grit mv advanced'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	mkdir -p sub/deep &&
	echo "top" >top.txt &&
	echo "sub" >sub/file.txt &&
	echo "deep" >sub/deep/file.txt &&
	git add . &&
	git commit -m "initial"
	)
'

# ── Move to/from subdirectories ─────────────────────────────────────────

test_expect_success 'mv file into subdirectory' '
	(
	cd repo &&
	git mv top.txt sub/ &&
	git ls-files >actual &&
	grep "sub/top.txt" actual &&
	! grep "^top.txt$" actual &&
	test -f sub/top.txt &&
	! test -f top.txt
	)
'

test_expect_success 'commit and reset for next test' '
	(
	cd repo &&
	git commit -m "moved to sub" &&
	git mv sub/top.txt top.txt &&
	git commit -m "moved back"
	)
'

test_expect_success 'mv file from subdir to top' '
	(
	cd repo &&
	git mv sub/file.txt moved-up.txt &&
	git ls-files >actual &&
	grep "^moved-up.txt$" actual &&
	! grep "^sub/file.txt$" actual &&
	test -f moved-up.txt
	)
'

test_expect_success 'commit and continue' '
	(
	cd repo &&
	git commit -m "moved up"
	)
'

test_expect_success 'mv file between subdirectories' '
	(
	cd repo &&
	mkdir -p other &&
	git mv sub/deep/file.txt other/file.txt &&
	git ls-files >actual &&
	grep "other/file.txt" actual &&
	! grep "sub/deep/file.txt" actual
	)
'

test_expect_success 'commit cross-subdir move' '
	(
	cd repo &&
	git commit -m "cross-subdir"
	)
'

# ── Move directories ─────────────────────────────────────────────────────

test_expect_success 'setup for directory moves' '
	(
	cd repo &&
	mkdir -p dir1 &&
	echo "a" >dir1/a.txt &&
	echo "b" >dir1/b.txt &&
	git add dir1 &&
	git commit -m "add dir1"
	)
'

test_expect_success 'mv directory to new name' '
	(
	cd repo &&
	git mv dir1 dir2 &&
	git ls-files >actual &&
	grep "dir2/a.txt" actual &&
	grep "dir2/b.txt" actual &&
	! grep "dir1/" actual &&
	test -f dir2/a.txt &&
	test -f dir2/b.txt
	)
'

test_expect_success 'commit dir rename' '
	(
	cd repo &&
	git commit -m "renamed dir"
	)
'

test_expect_success 'mv directory into another directory' '
	(
	cd repo &&
	mkdir -p target &&
	git mv dir2 target/ &&
	git ls-files >actual &&
	grep "target/dir2/a.txt" actual &&
	grep "target/dir2/b.txt" actual
	)
'

test_expect_success 'commit nested dir move' '
	(
	cd repo &&
	git commit -m "dir into dir"
	)
'

# ── -n dry-run ───────────────────────────────────────────────────────────

test_expect_success 'mv -n shows what would happen without moving' '
	(
	cd repo &&
	echo "dry" >dry.txt &&
	git add dry.txt &&
	git commit -m "add dry" &&
	git mv -n dry.txt dry-moved.txt &&
	test -f dry.txt &&
	! test -f dry-moved.txt &&
	git ls-files >actual &&
	grep "^dry.txt$" actual &&
	! grep "dry-moved.txt" actual
	)
'

test_expect_success 'mv -n with directory' '
	(
	cd repo &&
	mkdir -p ndir &&
	echo "n" >ndir/n.txt &&
	git add ndir &&
	git commit -m "add ndir" &&
	git mv -n ndir ndir-moved &&
	test -d ndir &&
	! test -d ndir-moved
	)
'

# ── -k skip errors ──────────────────────────────────────────────────────

test_expect_success 'mv -k skips files that cannot be moved' '
	(
	cd repo &&
	echo "ok" >movable.txt &&
	git add movable.txt &&
	git commit -m "add movable" &&
	mkdir -p dest &&
	git mv -k nonexistent.txt movable.txt dest/ 2>err &&
	git ls-files >actual &&
	grep "dest/movable.txt" actual
	)
'

test_expect_success 'mv -k with untracked source silently skips' '
	(
	cd repo &&
	echo "good" >good.txt &&
	git add good.txt &&
	git commit -m "setup -k skip" &&
	mkdir -p kdir &&
	git mv -k nonexistent.txt good.txt kdir/ 2>err &&
	git ls-files >actual &&
	grep "kdir/good.txt" actual
	)
'

# ── -f force ─────────────────────────────────────────────────────────────

test_expect_success 'mv -f overwrites destination in index' '
	(
	cd repo &&
	echo "alpha" >alpha.txt &&
	echo "beta" >beta.txt &&
	git add alpha.txt beta.txt &&
	git commit -m "two files" &&
	git mv -f alpha.txt beta.txt &&
	git ls-files >actual &&
	! grep "^alpha.txt$" actual &&
	grep "^beta.txt$" actual &&
	test "$(cat beta.txt)" = "alpha"
	)
'

test_expect_success 'commit forced mv' '
	(
	cd repo &&
	git commit -m "forced overwrite"
	)
'

# ── Multiple sources to directory ────────────────────────────────────────

test_expect_success 'mv multiple files to directory' '
	(
	cd repo &&
	echo "m1" >m1.txt &&
	echo "m2" >m2.txt &&
	echo "m3" >m3.txt &&
	git add m1.txt m2.txt m3.txt &&
	git commit -m "multi files" &&
	mkdir -p multi-dest &&
	git mv m1.txt m2.txt m3.txt multi-dest/ &&
	git ls-files >actual &&
	grep "multi-dest/m1.txt" actual &&
	grep "multi-dest/m2.txt" actual &&
	grep "multi-dest/m3.txt" actual
	)
'

test_expect_success 'commit multi-mv' '
	(
	cd repo &&
	git commit -m "multi mv"
	)
'

# ── -v verbose ───────────────────────────────────────────────────────────

test_expect_success 'mv -v shows rename info' '
	(
	cd repo &&
	echo "verbose" >verbose.txt &&
	git add verbose.txt &&
	git commit -m "add verbose" &&
	git mv -v verbose.txt verbose-moved.txt >out 2>&1 &&
	grep -i "verbose.txt" out
	)
'

test_expect_success 'commit verbose mv' '
	(
	cd repo &&
	git commit -m "verbose mv"
	)
'

# ── Error cases ──────────────────────────────────────────────────────────

test_expect_success 'mv fails for untracked file' '
	(
	cd repo &&
	echo "untracked" >untracked.txt &&
	test_must_fail git mv untracked.txt somewhere.txt 2>err
	)
'

test_expect_success 'mv fails when source does not exist' '
	(
	cd repo &&
	test_must_fail git mv no-such-file.txt dest.txt 2>err
	)
'

test_expect_success 'mv fails when destination is existing file without -f' '
	(
	cd repo &&
	echo "e1" >exist1.txt &&
	echo "e2" >exist2.txt &&
	git add exist1.txt exist2.txt &&
	git commit -m "exists" &&
	test_must_fail git mv exist1.txt exist2.txt 2>err
	)
'

# ── Filename with spaces ─────────────────────────────────────────────────

test_expect_success 'mv file with spaces in name' '
	(
	cd repo &&
	echo "spaces" >"file with spaces.txt" &&
	git add "file with spaces.txt" &&
	git commit -m "spaces" &&
	git mv "file with spaces.txt" "renamed spaces.txt" &&
	git ls-files >actual &&
	grep "renamed spaces.txt" actual &&
	! grep "file with spaces.txt" actual
	)
'

test_expect_success 'commit space mv' '
	(
	cd repo &&
	git commit -m "space rename"
	)
'

# ── Move and check working tree ──────────────────────────────────────────

test_expect_success 'mv updates working tree' '
	(
	cd repo &&
	echo "wt" >wt-file.txt &&
	git add wt-file.txt &&
	git commit -m "wt" &&
	git mv wt-file.txt wt-renamed.txt &&
	test -f wt-renamed.txt &&
	! test -f wt-file.txt &&
	test "$(cat wt-renamed.txt)" = "wt"
	)
'

test_expect_success 'commit wt mv' '
	(
	cd repo &&
	git commit -m "wt mv"
	)
'

test_done
