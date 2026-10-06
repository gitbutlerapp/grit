#!/bin/sh
# Advanced tests for 'grit rm': pathspec, -r deep, --cached, -n, -f, edge cases.

test_description='grit rm advanced'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	mkdir -p sub/deep/deeper &&
	echo "top" >top.txt &&
	echo "readme" >README.md &&
	echo "sub" >sub/file.txt &&
	echo "deep" >sub/deep/file.txt &&
	echo "deeper" >sub/deep/deeper/file.txt &&
	echo "code" >main.c &&
	echo "header" >main.h &&
	git add . &&
	git commit -m "initial"
	)
'

# ── Basic rm ─────────────────────────────────────────────────────────────

test_expect_success 'rm removes file from index and worktree' '
	(
	cd repo &&
	git rm top.txt &&
	! test -f top.txt &&
	git ls-files >actual &&
	! grep "^top.txt$" actual
	)
'

test_expect_success 'commit rm' '
	(
	cd repo &&
	git commit -m "rm top"
	)
'

# ── --cached: remove from index, keep in worktree ──────────────────────

test_expect_success 'rm --cached removes from index but keeps worktree file' '
	(
	cd repo &&
	git rm --cached README.md &&
	test -f README.md &&
	git ls-files >actual &&
	! grep "^README.md$" actual
	)
'

test_expect_success 'commit cached rm' '
	(
	cd repo &&
	git commit -m "rm cached README"
	)
'

# ── -r recursive removal ────────────────────────────────────────────────

test_expect_success 'rm -r removes directory recursively' '
	(
	cd repo &&
	git rm -r sub/ &&
	git ls-files >actual &&
	! grep "^sub/" actual &&
	! test -d sub
	)
'

test_expect_success 'commit recursive rm' '
	(
	cd repo &&
	git commit -m "rm -r sub"
	)
'

test_expect_success 'rm without -r fails on directory' '
	(
	cd repo &&
	mkdir -p dir2 &&
	echo "d" >dir2/d.txt &&
	git add dir2 &&
	git commit -m "add dir2" &&
	test_must_fail git rm dir2/ 2>err
	)
'

test_expect_success 'rm -r on deep nesting' '
	(
	cd repo &&
	mkdir -p a/b/c/d &&
	echo "leaf" >a/b/c/d/leaf.txt &&
	echo "mid" >a/b/mid.txt &&
	git add a &&
	git commit -m "deep tree" &&
	git rm -r a/ &&
	git ls-files >actual &&
	! grep "^a/" actual
	)
'

test_expect_success 'commit deep rm' '
	(
	cd repo &&
	git commit -m "rm deep"
	)
'

# ── -n dry-run ───────────────────────────────────────────────────────────

test_expect_success 'rm -n shows what would be removed without removing' '
	(
	cd repo &&
	echo "keep" >keep.txt &&
	git add keep.txt &&
	git commit -m "add keep" &&
	git rm -n keep.txt >out 2>&1 &&
	test -f keep.txt &&
	git ls-files >actual &&
	grep "^keep.txt$" actual
	)
'

test_expect_success 'rm -n with -r on directory' '
	(
	cd repo &&
	git rm -r dir2 &&
	git commit -m "cleanup dir2" &&
	mkdir -p drydir &&
	echo "d1" >drydir/one.txt &&
	echo "d2" >drydir/two.txt &&
	git add drydir &&
	git commit -m "add drydir" &&
	git rm -rn drydir/ >out 2>&1 &&
	test -d drydir &&
	git ls-files >actual &&
	grep "drydir/one.txt" actual
	)
'

# ── -f force ─────────────────────────────────────────────────────────────

test_expect_success 'rm -f removes file with local modifications' '
	(
	cd repo &&
	echo "original" >force.txt &&
	git add force.txt &&
	git commit -m "add force" &&
	echo "modified" >force.txt &&
	git rm -f force.txt &&
	! test -f force.txt &&
	git ls-files >actual &&
	! grep "^force.txt$" actual
	)
'

test_expect_success 'commit force rm' '
	(
	cd repo &&
	git commit -m "force rm"
	)
'

# ── -q quiet ─────────────────────────────────────────────────────────────

test_expect_success 'rm -q suppresses output' '
	(
	cd repo &&
	echo "quiet" >quiet.txt &&
	git add quiet.txt &&
	git commit -m "add quiet" &&
	git rm -q quiet.txt >out 2>&1 &&
	test_must_be_empty out
	)
'

test_expect_success 'commit quiet rm' '
	(
	cd repo &&
	git commit -m "quiet rm"
	)
'

# ── --ignore-unmatch ─────────────────────────────────────────────────────

test_expect_success 'rm --ignore-unmatch succeeds for nonexistent file' '
	(
	cd repo &&
	git rm --ignore-unmatch nonexistent.txt
	)
'

test_expect_success 'rm fails for nonexistent file without --ignore-unmatch' '
	(
	cd repo &&
	test_must_fail git rm nonexistent.txt 2>err
	)
'

# ── Multiple files ───────────────────────────────────────────────────────

test_expect_success 'rm multiple files at once' '
	(
	cd repo &&
	echo "m1" >multi1.txt &&
	echo "m2" >multi2.txt &&
	echo "m3" >multi3.txt &&
	git add multi1.txt multi2.txt multi3.txt &&
	git commit -m "multi" &&
	git rm multi1.txt multi2.txt multi3.txt &&
	git ls-files >actual &&
	! grep "multi" actual
	)
'

test_expect_success 'commit multi rm' '
	(
	cd repo &&
	git commit -m "multi rm"
	)
'

# ── --cached with worktree check ────────────────────────────────────────

test_expect_success 'rm --cached keeps multiple worktree files' '
	(
	cd repo &&
	echo "c1" >cached1.txt &&
	echo "c2" >cached2.txt &&
	git add cached1.txt cached2.txt &&
	git commit -m "cached files" &&
	git rm --cached cached1.txt cached2.txt &&
	test -f cached1.txt &&
	test -f cached2.txt &&
	git ls-files >actual &&
	! grep "cached1.txt" actual &&
	! grep "cached2.txt" actual
	)
'

test_expect_success 'commit cached multi rm' '
	(
	cd repo &&
	git commit -m "cached multi rm"
	)
'

# ── rm file in subdirectory ──────────────────────────────────────────────

test_expect_success 'rm file in subdirectory' '
	(
	cd repo &&
	mkdir -p subdir &&
	echo "s" >subdir/s.txt &&
	git add subdir &&
	git commit -m "add subdir" &&
	git rm subdir/s.txt &&
	! test -f subdir/s.txt &&
	git ls-files >actual &&
	! grep "subdir/s.txt" actual
	)
'

test_expect_success 'commit subdir rm' '
	(
	cd repo &&
	git commit -m "subdir rm"
	)
'

# ── rm after modification stages ─────────────────────────────────────────

test_expect_success 'rm --cached on staged new file' '
	(
	cd repo &&
	echo "new" >staged-new.txt &&
	git add staged-new.txt &&
	git rm --cached staged-new.txt &&
	test -f staged-new.txt &&
	git ls-files >actual &&
	! grep "staged-new.txt" actual
	)
'

# ── Pathspec with wildcard (shell glob) ──────────────────────────────────

test_expect_success 'rm multiple explicit pathspecs' '
	(
	cd repo &&
	echo "g1" >glob1.txt &&
	echo "g2" >glob2.txt &&
	echo "g3" >glob3.txt &&
	echo "other" >other.md &&
	git add glob1.txt glob2.txt glob3.txt other.md &&
	git commit -m "explicit paths" &&
	git rm glob1.txt glob2.txt glob3.txt &&
	git ls-files >actual &&
	! grep "glob" actual &&
	grep "other.md" actual
	)
'

test_expect_success 'commit explicit rm' '
	(
	cd repo &&
	git commit -m "explicit rm"
	)
'

# ── rm file with spaces ──────────────────────────────────────────────────

test_expect_success 'rm file with spaces in name' '
	(
	cd repo &&
	echo "sp" >"file with spaces.txt" &&
	git add "file with spaces.txt" &&
	git commit -m "spaces" &&
	git rm "file with spaces.txt" &&
	! test -f "file with spaces.txt" &&
	git ls-files >actual &&
	! grep "file with spaces" actual
	)
'

test_expect_success 'commit spaces rm' '
	(
	cd repo &&
	git commit -m "spaces rm"
	)
'

# ── Combined flags ───────────────────────────────────────────────────────

test_expect_success 'rm -rf on directory with local changes' '
	(
	cd repo &&
	mkdir -p fdir &&
	echo "f1" >fdir/f1.txt &&
	echo "f2" >fdir/f2.txt &&
	git add fdir &&
	git commit -m "add fdir" &&
	echo "changed" >fdir/f1.txt &&
	git rm -rf fdir/ &&
	! test -d fdir &&
	git ls-files >actual &&
	! grep "fdir/" actual
	)
'

test_expect_success 'commit combined rm' '
	(
	cd repo &&
	git commit -m "rf rm"
	)
'

test_done
