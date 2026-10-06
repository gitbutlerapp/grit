#!/bin/sh
# Tests for rm --cached with deep paths, -r, multiple files,
# .gitignore interaction, and edge cases.

test_description='rm --cached with deep paths and options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with deep structure' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	mkdir -p a/b/c/d &&
	echo "deep" >a/b/c/d/deep.txt &&
	echo "mid" >a/b/mid.txt &&
	echo "top" >top.txt &&
	echo "root" >root.txt &&
	git add . &&
	git commit -m "initial"
	)
'

# ── rm --cached basic ───────────────────────────────────────────────────────

test_expect_success 'rm --cached removes file from index' '
	(
	cd repo &&
	git rm --cached top.txt &&
	git ls-files >out &&
	! grep "top.txt" out
	)
'

test_expect_success 'rm --cached keeps file in working tree' '
	(
	cd repo &&
	test_path_is_file top.txt
	)
'

test_expect_success 'rm --cached on deep path' '
	(
	cd repo &&
	git rm --cached a/b/c/d/deep.txt &&
	git ls-files >out &&
	! grep "a/b/c/d/deep.txt" out
	)
'

test_expect_success 'rm --cached deep path keeps file on disk' '
	(
	cd repo &&
	test_path_is_file a/b/c/d/deep.txt
	)
'

test_expect_success 'rm --cached on mid-level path' '
	(
	cd repo &&
	git rm --cached a/b/mid.txt &&
	git ls-files >out &&
	! grep "a/b/mid.txt" out &&
	test_path_is_file a/b/mid.txt
	)
'

# ── rm --cached with multiple files ─────────────────────────────────────────

test_expect_success 'setup for multi-file rm' '
	(
	git init multi &&
	cd multi &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	echo "c" >c.txt &&
	echo "d" >d.txt &&
	git add . &&
	git commit -m "initial"
	)
'

test_expect_success 'rm --cached with multiple files' '
	(
	cd multi &&
	git rm --cached a.txt b.txt c.txt &&
	git ls-files >out &&
	! grep "a.txt" out &&
	! grep "b.txt" out &&
	! grep "c.txt" out
	)
'

test_expect_success 'rm --cached multiple keeps them on disk' '
	(
	cd multi &&
	test_path_is_file a.txt &&
	test_path_is_file b.txt &&
	test_path_is_file c.txt
	)
'

test_expect_success 'rm --cached multiple preserves other files in index' '
	(
	cd multi &&
	git ls-files >out &&
	grep "d.txt" out
	)
'

# ── rm --cached -r (recursive) ──────────────────────────────────────────────

test_expect_success 'setup for recursive rm' '
	(
	git init recur &&
	cd recur &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	mkdir -p dir1/sub1 dir1/sub2 dir2 &&
	echo "a" >dir1/sub1/a.txt &&
	echo "b" >dir1/sub1/b.txt &&
	echo "c" >dir1/sub2/c.txt &&
	echo "d" >dir2/d.txt &&
	echo "e" >root.txt &&
	git add . &&
	git commit -m "initial"
	)
'

test_expect_success 'rm --cached -r removes directory recursively' '
	(
	cd recur &&
	git rm --cached -r dir1 &&
	git ls-files >out &&
	! grep "dir1" out
	)
'

test_expect_success 'rm --cached -r keeps files on disk' '
	(
	cd recur &&
	test_path_is_file dir1/sub1/a.txt &&
	test_path_is_file dir1/sub1/b.txt &&
	test_path_is_file dir1/sub2/c.txt
	)
'

test_expect_success 'rm --cached -r preserves other directories' '
	(
	cd recur &&
	git ls-files >out &&
	grep "dir2/d.txt" out &&
	grep "root.txt" out
	)
'

test_expect_success 'rm -r without --cached removes from disk too' '
	(
	cd recur &&
	git rm -r dir2 &&
	test_path_is_missing dir2/d.txt &&
	git ls-files >out &&
	! grep "dir2" out
	)
'

# ── rm with --force ─────────────────────────────────────────────────────────

test_expect_success 'setup for force rm' '
	(
	git init forcerepo &&
	cd forcerepo &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "original" >modified.txt &&
	git add modified.txt &&
	git commit -m "initial" &&
	echo "changed" >modified.txt
	)
'

test_expect_success 'rm --cached on modified file removes from index' '
	(
	cd forcerepo &&
	git rm --cached modified.txt &&
	git ls-files >out &&
	! grep "modified.txt" out
	)
'

# ── rm --dry-run ────────────────────────────────────────────────────────────

test_expect_success 'setup for dry-run rm' '
	(
	git init dryrepo &&
	cd dryrepo &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "a" >dry1.txt &&
	echo "b" >dry2.txt &&
	git add . &&
	git commit -m "initial"
	)
'

test_expect_success 'rm --cached -n shows what would be removed' '
	(
	cd dryrepo &&
	git rm --cached -n dry1.txt >out 2>&1 &&
	grep "dry1.txt" out
	)
'

test_expect_success 'rm --cached -n does not actually remove' '
	(
	cd dryrepo &&
	git ls-files >out &&
	grep "dry1.txt" out
	)
'

# ── rm --quiet ──────────────────────────────────────────────────────────────

test_expect_success 'rm --cached -q suppresses output' '
	(
	cd dryrepo &&
	git rm --cached -q dry1.txt >out 2>&1 &&
	test_must_be_empty out
	)
'

# ── rm --ignore-unmatch ─────────────────────────────────────────────────────

test_expect_success 'rm nonexistent file fails' '
	(
	cd dryrepo &&
	test_must_fail git rm --cached nonexistent.txt
	)
'

test_expect_success 'rm --ignore-unmatch on nonexistent file succeeds' '
	(
	cd dryrepo &&
	git rm --cached --ignore-unmatch nonexistent.txt
	)
'

# ── rm --cached then re-add ─────────────────────────────────────────────────

test_expect_success 'rm --cached then re-add restores index entry' '
	(
	cd dryrepo &&
	git rm --cached dry2.txt &&
	git ls-files >out &&
	! grep "dry2.txt" out &&
	git add dry2.txt &&
	git ls-files >out &&
	grep "dry2.txt" out
	)
'

# ── rm --cached with deeply nested structure ─────────────────────────────────

test_expect_success 'setup very deep nesting' '
	(
	git init deeprepo &&
	cd deeprepo &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	mkdir -p x/y/z/w/v &&
	echo "leaf" >x/y/z/w/v/leaf.txt &&
	echo "mid" >x/y/z/mid.txt &&
	echo "near" >x/near.txt &&
	git add . &&
	git commit -m "deep initial"
	)
'

test_expect_success 'rm --cached -r from middle of tree' '
	(
	cd deeprepo &&
	git rm --cached -r x/y/z &&
	git ls-files >out &&
	! grep "x/y/z" out &&
	grep "x/near.txt" out
	)
'

test_expect_success 'rm --cached -r from top of tree' '
	(
	cd deeprepo &&
	git rm --cached -r x &&
	git ls-files >out &&
	! grep "x/" out
	)
'

test_done
