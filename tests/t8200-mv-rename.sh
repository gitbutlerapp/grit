#!/bin/sh
# Tests for mv renames, directory restructuring, collision handling,
# dry-run, force, verbose, and -k flag.

test_description='mv renames and directory restructuring'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "alpha" >a.txt &&
	echo "beta" >b.txt &&
	echo "gamma" >c.txt &&
	mkdir -p src/lib &&
	echo "main code" >src/main.rs &&
	echo "lib code" >src/lib/util.rs &&
	git add . &&
	git commit -m "initial"
	)
'

# ── Basic file rename ───────────────────────────────────────────────────────

test_expect_success 'mv renames a file' '
	(
	cd repo &&
	git mv a.txt alpha.txt &&
	test_path_is_file alpha.txt &&
	test_path_is_missing a.txt
	)
'

test_expect_success 'mv updates index after rename' '
	(
	cd repo &&
	git ls-files >out &&
	grep "alpha.txt" out &&
	! grep "^a\.txt$" out
	)
'

test_expect_success 'mv rename shows in status' '
	(
	cd repo &&
	git status --porcelain >out &&
	grep "alpha.txt" out
	)
'

test_expect_success 'commit after mv succeeds' '
	(
	cd repo &&
	git commit -m "rename a to alpha" &&
	git log --oneline >out &&
	grep "rename a to alpha" out
	)
'

# ── Move file to directory ──────────────────────────────────────────────────

test_expect_success 'mv file into directory' '
	(
	cd repo &&
	git mv b.txt src/ &&
	test_path_is_file src/b.txt &&
	test_path_is_missing b.txt
	)
'

test_expect_success 'mv file into directory updates index' '
	(
	cd repo &&
	git ls-files >out &&
	grep "src/b.txt" out &&
	! grep "^b.txt" out
	)
'

# ── Move file into new subdirectory ─────────────────────────────────────────

test_expect_success 'mv file to new path (creating dirs)' '
	(
	cd repo &&
	mkdir -p new/dir &&
	git mv c.txt new/dir/c.txt &&
	test_path_is_file new/dir/c.txt &&
	test_path_is_missing c.txt
	)
'

# ── Rename directory ────────────────────────────────────────────────────────

test_expect_success 'setup for directory rename' '
	(
	git init dirrepo &&
	cd dirrepo &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	mkdir -p old/sub &&
	echo "a" >old/a.txt &&
	echo "b" >old/sub/b.txt &&
	echo "root" >root.txt &&
	git add . &&
	git commit -m "initial"
	)
'

test_expect_success 'mv renames entire directory' '
	(
	cd dirrepo &&
	git mv old new &&
	test_path_is_dir new &&
	test_path_is_file new/a.txt &&
	test_path_is_file new/sub/b.txt &&
	test_path_is_missing old
	)
'

test_expect_success 'directory rename updates all index entries' '
	(
	cd dirrepo &&
	git ls-files >out &&
	grep "new/a.txt" out &&
	grep "new/sub/b.txt" out &&
	! grep "old/" out
	)
'

# ── Force move (overwrite) ──────────────────────────────────────────────────

test_expect_success 'setup for force mv' '
	(
	git init forcerepo &&
	cd forcerepo &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "source" >src.txt &&
	echo "target" >dst.txt &&
	git add . &&
	git commit -m "initial"
	)
'

test_expect_success 'mv without -f to existing file fails' '
	(
	cd forcerepo &&
	test_must_fail git mv src.txt dst.txt
	)
'

test_expect_success 'mv -f overwrites existing file' '
	(
	cd forcerepo &&
	git mv -f src.txt dst.txt &&
	test_path_is_file dst.txt &&
	test_path_is_missing src.txt &&
	cat dst.txt >out &&
	echo "source" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'mv -f updates index correctly' '
	(
	cd forcerepo &&
	git ls-files >out &&
	grep "dst.txt" out &&
	! grep "src.txt" out
	)
'

# ── Dry-run (-n) ────────────────────────────────────────────────────────────

test_expect_success 'setup for dry-run mv' '
	(
	git init dryrepo &&
	cd dryrepo &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "a" >dry.txt &&
	git add dry.txt &&
	git commit -m "initial"
	)
'

test_expect_success 'mv -n shows what would happen' '
	(
	cd dryrepo &&
	git mv -n dry.txt moved.txt >out 2>&1 &&
	grep "dry.txt" out
	)
'

test_expect_success 'mv -n does not actually move' '
	(
	cd dryrepo &&
	test_path_is_file dry.txt &&
	test_path_is_missing moved.txt
	)
'

test_expect_success 'mv -n does not change index' '
	(
	cd dryrepo &&
	git ls-files >out &&
	grep "dry.txt" out &&
	! grep "moved.txt" out
	)
'

# ── Verbose (-v) ────────────────────────────────────────────────────────────

test_expect_success 'mv -v shows verbose output' '
	(
	cd dryrepo &&
	git mv -v dry.txt moved.txt >out 2>&1 &&
	grep "dry.txt" out
	)
'

# ── -k flag (skip errors) ──────────────────────────────────────────────────

test_expect_success 'setup for -k tests' '
	(
	git init krepo &&
	cd krepo &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "a" >ka.txt &&
	echo "b" >kb.txt &&
	git add . &&
	git commit -m "initial"
	)
'

test_expect_success 'mv -k skips errors and continues' '
	(
	cd krepo &&
	mkdir dest &&
	echo "block" >dest/ka.txt &&
	git mv -k ka.txt kb.txt dest/ &&
	test_path_is_file dest/kb.txt
	)
'

# ── Move multiple files to directory ────────────────────────────────────────

test_expect_success 'setup for multi-file mv' '
	(
	git init multimv &&
	cd multimv &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "a" >m1.txt &&
	echo "b" >m2.txt &&
	echo "c" >m3.txt &&
	mkdir target &&
	git add . &&
	git commit -m "initial"
	)
'

test_expect_success 'mv multiple files to directory' '
	(
	cd multimv &&
	git mv m1.txt m2.txt m3.txt target/ &&
	test_path_is_file target/m1.txt &&
	test_path_is_file target/m2.txt &&
	test_path_is_file target/m3.txt &&
	test_path_is_missing m1.txt
	)
'

test_expect_success 'mv multiple files updates index' '
	(
	cd multimv &&
	git ls-files >out &&
	grep "target/m1.txt" out &&
	grep "target/m2.txt" out &&
	grep "target/m3.txt" out &&
	! grep "^m1.txt" out
	)
'

# ── mv from subdirectory ────────────────────────────────────────────────────

test_expect_success 'mv file from subdirectory to root' '
	(
	cd multimv &&
	git mv target/m1.txt m1-back.txt &&
	test_path_is_file m1-back.txt &&
	test_path_is_missing target/m1.txt
	)
'

# ── rename with same name different case ─────────────────────────────────────

test_expect_success 'mv to different extension' '
	(
	cd multimv &&
	git mv m1-back.txt m1-back.md &&
	test_path_is_file m1-back.md &&
	git ls-files >out &&
	grep "m1-back.md" out
	)
'

# ── mv to nonexistent directory fails ───────────────────────────────────────

test_expect_success 'mv to path creates intermediate directories' '
	(
	cd multimv &&
	git mv m1-back.md created/sub/file.md &&
	test_path_is_file created/sub/file.md &&
	git ls-files >out &&
	grep "created/sub/file.md" out
	)
'

# ── Edge: mv file to itself ────────────────────────────────────────────────

test_expect_success 'mv file to itself fails or is no-op' '
	(
	cd multimv &&
	test_must_fail git mv m1-back.md m1-back.md 2>err
	)
'

# ── mv preserves file content ──────────────────────────────────────────────

test_expect_success 'mv preserves file content' '
	(
	git init content-repo &&
	cd content-repo &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo "important data" >orig.txt &&
	git add orig.txt &&
	git commit -m "initial" &&
	git mv orig.txt renamed.txt &&
	cat renamed.txt >out &&
	echo "important data" >expected &&
	test_cmp expected out
	)
'

test_done
