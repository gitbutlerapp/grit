#!/bin/sh
# Tests for add --intent-to-add/-N, pathspec handling, -u, -A, -f,
# -n (dry-run), and -v (verbose).

test_description='add intent-to-add, pathspec, and flags'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com"
	)
'

# ── Intent-to-add (-N / --intent-to-add) ────────────────────────────────────

test_expect_success 'add -N records intent-to-add' '
	(
	cd repo &&
	echo "new content" >intent.txt &&
	git add -N intent.txt &&
	git ls-files >out &&
	grep "intent.txt" out
	)
'

test_expect_success 'intent-to-add file shows in status as new' '
	(
	cd repo &&
	git status --porcelain >out &&
	grep "intent.txt" out
	)
'

test_expect_success 'add -N on multiple files' '
	(
	cd repo &&
	echo "a" >ia.txt &&
	echo "b" >ib.txt &&
	echo "c" >ic.txt &&
	git add -N ia.txt ib.txt ic.txt &&
	git ls-files >out &&
	grep "ia.txt" out &&
	grep "ib.txt" out &&
	grep "ic.txt" out
	)
'

test_expect_success 'intent-to-add then full add works' '
	(
	cd repo &&
	git add intent.txt &&
	git diff --cached --name-only >out &&
	grep "intent.txt" out
	)
'

test_expect_success 'commit after intent-to-add then add succeeds' '
	(
	cd repo &&
	git add ia.txt ib.txt ic.txt &&
	git commit -m "add intent files" &&
	git log --oneline >out &&
	grep "add intent files" out
	)
'

# ── Pathspec with add ────────────────────────────────────────────────────────

test_expect_success 'add specific files by name' '
	(
	cd repo &&
	echo "x" >glob1.txt &&
	echo "y" >glob2.txt &&
	echo "z" >glob3.log &&
	git add glob1.txt glob2.txt &&
	git ls-files --cached >out &&
	grep "glob1.txt" out &&
	grep "glob2.txt" out
	)
'

test_expect_success 'add specific files does not add others' '
	(
	cd repo &&
	git ls-files --cached >out &&
	! grep "glob3.log" out
	)
'

test_expect_success 'add with dot adds everything' '
	(
	cd repo &&
	echo "all1" >all1.dat &&
	echo "all2" >all2.dat &&
	git add . &&
	git ls-files --cached >out &&
	grep "all1.dat" out &&
	grep "all2.dat" out &&
	grep "glob3.log" out
	)
'

test_expect_success 'add specific file path' '
	(
	cd repo &&
	echo "specific" >specific.txt &&
	git add specific.txt &&
	git ls-files --cached >out &&
	grep "specific.txt" out
	)
'

test_expect_success 'add file in subdirectory' '
	(
	cd repo &&
	mkdir -p sub/deep &&
	echo "nested" >sub/deep/file.txt &&
	git add sub/deep/file.txt &&
	git ls-files --cached >out &&
	grep "sub/deep/file.txt" out
	)
'

test_expect_success 'add directory recursively' '
	(
	cd repo &&
	mkdir -p adddir &&
	echo "a" >adddir/a.txt &&
	echo "b" >adddir/b.txt &&
	echo "c" >adddir/c.txt &&
	git add adddir &&
	git ls-files --cached >out &&
	grep "adddir/a.txt" out &&
	grep "adddir/b.txt" out &&
	grep "adddir/c.txt" out
	)
'

# ── --update (-u) ───────────────────────────────────────────────────────────

test_expect_success 'setup for -u tests' '
	(
	cd repo &&
	git commit -m "snapshot" -a &&
	echo "modified" >glob1.txt &&
	echo "newfile" >untracked.txt
	)
'

test_expect_success 'add -u stages modifications of tracked files' '
	(
	cd repo &&
	git add -u &&
	git diff --cached --name-only >out &&
	grep "glob1.txt" out
	)
'

test_expect_success 'add -u does not add untracked files' '
	(
	cd repo &&
	git diff --cached --name-only >out &&
	! grep "untracked.txt" out
	)
'

test_expect_success 'add -u stages deletions' '
	(
	cd repo &&
	rm glob2.txt &&
	git add -u &&
	git diff --cached --name-only >out &&
	grep "glob2.txt" out
	)
'

# ── --all (-A) ──────────────────────────────────────────────────────────────

test_expect_success 'setup for -A tests' '
	(
	git init allrepo &&
	cd allrepo &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "tracked" >tracked.txt &&
	git add tracked.txt &&
	git commit -m "initial" &&
	echo "modified tracked" >tracked.txt &&
	echo "brand new" >newfile.txt &&
	rm tracked.txt
	)
'

test_expect_success 'add -A stages new files and deletions' '
	(
	cd allrepo &&
	git add -A &&
	git diff --cached --name-only >out &&
	grep "newfile.txt" out &&
	grep "tracked.txt" out
	)
'

# ── --dry-run (-n) ──────────────────────────────────────────────────────────

test_expect_success 'setup for dry-run tests' '
	(
	git init dryrepo &&
	cd dryrepo &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "a" >dry1.txt &&
	echo "b" >dry2.txt
	)
'

test_expect_success 'add -n shows what would be added' '
	(
	cd dryrepo &&
	git add -n dry1.txt dry2.txt >out 2>&1 &&
	grep "dry1.txt" out &&
	grep "dry2.txt" out
	)
'

test_expect_success 'add -n does not actually add to index' '
	(
	cd dryrepo &&
	git ls-files >out &&
	! grep "dry1.txt" out &&
	! grep "dry2.txt" out
	)
'

# ── --verbose (-v) ──────────────────────────────────────────────────────────

test_expect_success 'add -v shows added files' '
	(
	cd dryrepo &&
	git add -v dry1.txt >out 2>&1 &&
	grep "dry1.txt" out
	)
'

# ── --force (-f) with .gitignore ─────────────────────────────────────────────

test_expect_success 'setup for force-add tests' '
	(
	git init forcerepo &&
	cd forcerepo &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "*.log" >.gitignore &&
	git add .gitignore &&
	echo "should be ignored" >debug.log
	)
'

test_expect_success 'add -f adds ignored file' '
	(
	cd forcerepo &&
	git add -f debug.log &&
	git ls-files >out &&
	grep "debug.log" out
	)
'

test_expect_success 'add -f adds another ignored file' '
	(
	cd forcerepo &&
	echo "another" >trace.log &&
	git add -f trace.log &&
	git ls-files >out &&
	grep "trace.log" out
	)
'

# ── Multiple pathspecs ──────────────────────────────────────────────────────

test_expect_success 'add with multiple explicit paths' '
	(
	git init multipaths &&
	cd multipaths &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	echo "c" >c.txt &&
	git add a.txt c.txt &&
	git ls-files >out &&
	grep "a.txt" out &&
	grep "c.txt" out &&
	! grep "b.txt" out
	)
'

# ── Edge cases ──────────────────────────────────────────────────────────────

test_expect_success 'add already-tracked unchanged file is a no-op' '
	(
	cd repo &&
	git add specific.txt
	)
'

test_expect_success 'add non-existent file fails' '
	(
	cd repo &&
	test_must_fail git add nonexistent.txt 2>err
	)
'

test_expect_success 'add empty directory is a no-op' '
	(
	cd repo &&
	mkdir -p emptydir &&
	git add emptydir 2>err || true &&
	git ls-files >out &&
	! grep "emptydir" out
	)
'

test_done
