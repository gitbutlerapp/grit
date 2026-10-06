#!/bin/sh
# Tests for grit status with untracked directories and -u (--untracked-files).
# Covers -u normal (default: collapse directories), -u all (show all files),
# -u no (hide untracked), and various directory structures.

test_description='status with untracked directories and -u options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with tracked files' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo "tracked" >file.txt &&
	mkdir -p src &&
	echo "code" >src/main.c &&
	git add . &&
	git commit -m "initial"
	)
'

# ── Default untracked behavior (normal) ─────────────────────────────────────

test_expect_success 'untracked file shows as ??' '
	(
	cd repo &&
	echo "loose" >loose.txt &&
	git status --porcelain >../actual &&
	grep "^?? loose.txt" ../actual
	)
'

test_expect_success 'untracked directory collapses to dir/' '
	(
	cd repo &&
	mkdir -p newdir &&
	echo "a" >newdir/a.txt &&
	echo "b" >newdir/b.txt &&
	git status --porcelain >../actual &&
	grep "^?? newdir/" ../actual
	)
'

test_expect_success 'nested untracked directory collapses to top dir/' '
	(
	cd repo &&
	mkdir -p newdir/sub &&
	echo "c" >newdir/sub/c.txt &&
	git status --porcelain >../actual &&
	grep "^?? newdir/" ../actual &&
	! grep "newdir/sub/" ../actual
	)
'

test_expect_success 'multiple untracked directories each show as dir/' '
	(
	cd repo &&
	mkdir -p extra docs &&
	echo "x" >extra/x.txt &&
	echo "y" >docs/y.md &&
	git status --porcelain >../actual &&
	grep "^?? extra/" ../actual &&
	grep "^?? docs/" ../actual &&
	grep "^?? newdir/" ../actual
	)
'

test_expect_success 'untracked files and dirs sorted in output' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	grep -v "^##" ../actual >../filtered &&
	sort ../filtered >../sorted &&
	test_cmp ../sorted ../filtered
	)
'

# ── -u no (hide all untracked) ──────────────────────────────────────────────

test_expect_success '-u no hides all untracked files' '
	(
	cd repo &&
	git status --porcelain -u no >../actual &&
	! grep "^??" ../actual
	)
'

test_expect_success '-u no hides untracked directories' '
	(
	cd repo &&
	git status --porcelain -u no >../actual &&
	! grep "newdir" ../actual &&
	! grep "extra" ../actual &&
	! grep "docs" ../actual
	)
'

test_expect_success '-u no with staged changes still shows staged' '
	(
	cd repo &&
	echo "modified" >>file.txt &&
	git add file.txt &&
	git status --porcelain -u no >../actual &&
	grep "^M  file.txt" ../actual &&
	! grep "^??" ../actual
	)
'

test_expect_success 'commit staged change' '
	(
	cd repo &&
	git commit -m "modify file.txt"
	)
'

test_expect_success '-u no with both staged and unstaged' '
	(
	cd repo &&
	echo "staged" >>src/main.c &&
	git add src/main.c &&
	echo "unstaged" >>file.txt &&
	git status --porcelain -u no >../actual &&
	grep "^M  src/main.c" ../actual &&
	grep "^ M file.txt" ../actual &&
	! grep "^??" ../actual
	)
'

test_expect_success 'commit and reset for next tests' '
	(
	cd repo &&
	git add file.txt &&
	git commit -m "more changes"
	)
'

# ── -u all (show all individual files) ──────────────────────────────────────

test_expect_success '-u all shows individual files in untracked dirs' '
	(
	cd repo &&
	git status --porcelain -u all >../actual &&
	grep "^?? newdir/a.txt" ../actual &&
	grep "^?? newdir/b.txt" ../actual &&
	grep "^?? newdir/sub/c.txt" ../actual
	)
'

test_expect_success '-u all shows files in all untracked dirs' '
	(
	cd repo &&
	git status --porcelain -u all >../actual &&
	grep "^?? extra/x.txt" ../actual &&
	grep "^?? docs/y.md" ../actual
	)
'

test_expect_success '-u all does not collapse directories' '
	(
	cd repo &&
	git status --porcelain -u all >../actual &&
	! grep "^?? newdir/$" ../actual &&
	! grep "^?? extra/$" ../actual
	)
'

test_expect_success '-u all still shows loose untracked files' '
	(
	cd repo &&
	git status --porcelain -u all >../actual &&
	grep "^?? loose.txt" ../actual
	)
'

# ── -u normal (explicit default) ────────────────────────────────────────────

test_expect_success '-u normal collapses directories (same as default)' '
	(
	cd repo &&
	git status --porcelain -u normal >../actual &&
	grep "^?? newdir/" ../actual &&
	grep "^?? loose.txt" ../actual
	)
'

test_expect_success '-u normal output matches default' '
	(
	cd repo &&
	git status --porcelain >../default &&
	git status --porcelain -u normal >../explicit &&
	test_cmp ../default ../explicit
	)
'

# ── Mixed tracked and untracked in same directory ────────────────────────────

test_expect_success 'untracked file in tracked directory shows individually' '
	(
	cd repo &&
	echo "new" >src/helper.c &&
	git status --porcelain >../actual &&
	grep "^?? src/helper.c" ../actual
	)
'

test_expect_success 'tracked dir with untracked file does not collapse' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	! grep "^?? src/$" ../actual &&
	grep "^?? src/helper.c" ../actual
	)
'

test_expect_success 'add and commit the helper file' '
	(
	cd repo &&
	git add src/helper.c &&
	git commit -m "add helper"
	)
'

# ── Empty untracked directory ────────────────────────────────────────────────

test_expect_success 'empty untracked directory is not shown' '
	(
	cd repo &&
	mkdir -p emptydir &&
	git status --porcelain >../actual &&
	! grep "emptydir" ../actual
	)
'

test_expect_success 'directory becomes visible when file added inside' '
	(
	cd repo &&
	echo "content" >emptydir/file.txt &&
	git status --porcelain >../actual &&
	grep "^?? emptydir/" ../actual
	)
'

# ── Deeply nested untracked structure ────────────────────────────────────────

test_expect_success 'deeply nested untracked dir collapses to top' '
	(
	cd repo &&
	mkdir -p deep/a/b/c/d &&
	echo "deep" >deep/a/b/c/d/file.txt &&
	git status --porcelain >../actual &&
	grep "^?? deep/" ../actual &&
	! grep "deep/a/" ../actual
	)
'

# ── Short format ─────────────────────────────────────────────────────────────

test_expect_success 'short format shows untracked dirs same as porcelain' '
	(
	cd repo &&
	git status --short >../actual &&
	grep "?? newdir/" ../actual &&
	grep "?? loose.txt" ../actual
	)
'

test_expect_success 'short format with -u no hides untracked' '
	(
	cd repo &&
	git status --short -u no >../actual &&
	! grep "^??" ../actual
	)
'

# ── Staging some untracked and rechecking ────────────────────────────────────

test_expect_success 'staging a file removes it from untracked' '
	(
	cd repo &&
	git add loose.txt &&
	git status --porcelain >../actual &&
	grep "^A  loose.txt" ../actual &&
	! grep "^?? loose.txt" ../actual
	)
'

test_expect_success 'staging all files in dir removes dir from untracked' '
	(
	cd repo &&
	git add newdir/ &&
	git status --porcelain >../actual &&
	! grep "^?? newdir/" ../actual &&
	grep "^A  newdir/a.txt" ../actual
	)
'

test_expect_success 'commit all staged' '
	(
	cd repo &&
	git add . &&
	git commit -m "add everything"
	)
'

# ── Clean status with no untracked ──────────────────────────────────────────

test_expect_success 'clean status shows no untracked entries' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	! grep "^??" ../actual
	)
'

test_expect_success 'clean short status has no ?? lines' '
	(
	cd repo &&
	git status --short >../actual &&
	! grep "^??" ../actual
	)
'

# ── New untracked after clean ────────────────────────────────────────────────

test_expect_success 'new untracked file appears after clean state' '
	(
	cd repo &&
	echo "brand new" >fresh.txt &&
	git status --porcelain >../actual &&
	grep "^?? fresh.txt" ../actual
	)
'

test_expect_success 'cleanup' '
	(
	cd repo &&
	rm -f fresh.txt
	)
'

test_done
