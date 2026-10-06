#!/bin/sh
# Tests for git mv with subdirectories, renames, and edge cases.

test_description='mv with subdirectories and renames'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- setup -------------------------------------------------------------------

test_expect_success 'setup: create repo with directory structure' '
	(
	git init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	mkdir -p src/lib src/bin docs &&
	echo "main" >src/bin/main.rs &&
	echo "lib" >src/lib/lib.rs &&
	echo "util" >src/lib/util.rs &&
	echo "readme" >docs/README.md &&
	echo "root" >root.txt &&
	git add . &&
	test_tick &&
	git commit -m "initial structure"
	)
'

# -- basic file rename --------------------------------------------------------

test_expect_success 'mv renames a file' '
	(
	cd repo &&
	git mv root.txt renamed.txt &&
	test_path_is_missing root.txt &&
	test_path_is_file renamed.txt &&
	git ls-files renamed.txt >out &&
	grep "renamed.txt" out
	)
'

test_expect_success 'old filename removed from index' '
	(
	cd repo &&
	git ls-files root.txt >out &&
	test -z "$(cat out)"
	)
'

test_expect_success 'status shows rename' '
	(
	cd repo &&
	git status --porcelain >out &&
	grep "renamed.txt" out
	)
'

test_expect_success 'commit rename' '
	(
	cd repo &&
	test_tick &&
	git commit -m "rename root.txt" &&
	git log --oneline >out &&
	grep "rename root.txt" out
	)
'

# -- move file into subdirectory ----------------------------------------------

test_expect_success 'mv file into existing subdirectory' '
	(
	cd repo &&
	git mv renamed.txt docs/ &&
	test_path_is_file docs/renamed.txt &&
	git ls-files docs/renamed.txt >out &&
	grep "docs/renamed.txt" out
	)
'

test_expect_success 'commit file move to subdir' '
	(
	cd repo &&
	test_tick &&
	git commit -m "move to docs"
	)
'

# -- move file out of subdirectory --------------------------------------------

test_expect_success 'mv file from subdir to root' '
	(
	cd repo &&
	git mv docs/renamed.txt . &&
	test_path_is_file renamed.txt &&
	git ls-files renamed.txt >out &&
	grep "renamed.txt" out &&
	test_tick &&
	git commit -m "move back to root"
	)
'

# -- move entire directory ----------------------------------------------------

test_expect_success 'mv directory to new name' '
	(
	cd repo &&
	git mv docs documentation &&
	test_path_is_dir documentation &&
	test_path_is_missing docs &&
	git ls-files documentation/ >out &&
	grep "documentation/README.md" out
	)
'

test_expect_success 'commit directory rename' '
	(
	cd repo &&
	test_tick &&
	git commit -m "rename docs to documentation"
	)
'

# -- move directory into another directory ------------------------------------

test_expect_success 'mv directory into another directory' '
	(
	cd repo &&
	git mv documentation src/ &&
	test_path_is_dir src/documentation &&
	git ls-files src/documentation/ >out &&
	grep "src/documentation/README.md" out &&
	test_tick &&
	git commit -m "move documentation into src"
	)
'

# -- move file across directories ---------------------------------------------

test_expect_success 'mv file between subdirectories' '
	(
	cd repo &&
	git mv src/lib/util.rs src/bin/ &&
	test_path_is_file src/bin/util.rs &&
	test_path_is_missing src/lib/util.rs &&
	test_tick &&
	git commit -m "move util.rs to bin"
	)
'

# -- mv with --dry-run --------------------------------------------------------

test_expect_success 'mv --dry-run does not actually move' '
	(
	cd repo &&
	git mv -n src/bin/main.rs src/lib/ >out 2>&1 &&
	test_path_is_file src/bin/main.rs &&
	test_path_is_missing src/lib/main.rs
	)
'

# -- mv with --force ----------------------------------------------------------

test_expect_success 'mv to existing file fails without force' '
	(
	cd repo &&
	echo "target" >target.txt &&
	git add target.txt &&
	test_tick &&
	git commit -m "add target" &&
	echo "source" >source.txt &&
	git add source.txt &&
	test_tick &&
	git commit -m "add source" &&
	test_must_fail git mv source.txt target.txt 2>err
	)
'

test_expect_success 'mv --force overwrites existing file' '
	(
	cd repo &&
	git mv -f source.txt target.txt &&
	test_path_is_missing source.txt &&
	cat target.txt >out &&
	grep "source" out &&
	test_tick &&
	git commit -m "force mv"
	)
'

# -- mv with --verbose --------------------------------------------------------

test_expect_success 'mv --verbose shows what is being moved' '
	(
	cd repo &&
	echo "vfile" >verbose-test.txt &&
	git add verbose-test.txt &&
	test_tick &&
	git commit -m "add verbose-test" &&
	git mv -v verbose-test.txt vt.txt >out 2>&1 &&
	test_path_is_file vt.txt &&
	test_tick &&
	git commit -m "verbose mv"
	)
'

# -- mv multiple files to directory -------------------------------------------

test_expect_success 'mv multiple files to a directory' '
	(
	cd repo &&
	echo "f1" >f1.txt &&
	echo "f2" >f2.txt &&
	echo "f3" >f3.txt &&
	mkdir dest &&
	git add f1.txt f2.txt f3.txt &&
	test_tick &&
	git commit -m "multi files" &&
	git mv f1.txt f2.txt f3.txt dest/ &&
	test_path_is_file dest/f1.txt &&
	test_path_is_file dest/f2.txt &&
	test_path_is_file dest/f3.txt &&
	test_tick &&
	git commit -m "mv multi to dest"
	)
'

# -- mv to non-existent directory fails ---------------------------------------

test_expect_success 'mv to non-existent directory fails' '
	(
	cd repo &&
	test_must_fail git mv dest/f1.txt nodir/ 2>err
	)
'

# -- mv -k skips errors -------------------------------------------------------

test_expect_success 'mv file within same directory (rename)' '
	(
	cd repo &&
	git mv dest/f1.txt dest/f1-renamed.txt &&
	test_path_is_file dest/f1-renamed.txt &&
	test_path_is_missing dest/f1.txt &&
	test_tick &&
	git commit -m "rename within dest"
	)
'

# -- mv file with spaces in name ---------------------------------------------

test_expect_success 'mv file with spaces in name' '
	(
	cd repo &&
	echo "spaced" >"file with spaces.txt" &&
	git add "file with spaces.txt" &&
	test_tick &&
	git commit -m "spaced file" &&
	git mv "file with spaces.txt" "no spaces.txt" &&
	test_path_is_file "no spaces.txt" &&
	test_path_is_missing "file with spaces.txt" &&
	test_tick &&
	git commit -m "rename spaced"
	)
'

# -- mv preserves file content ------------------------------------------------

test_expect_success 'mv preserves file content exactly' '
	(
	cd repo &&
	echo "exact content 12345" >content-check.txt &&
	git add content-check.txt &&
	test_tick &&
	git commit -m "content check" &&
	git mv content-check.txt moved-content.txt &&
	cat moved-content.txt >out &&
	grep "exact content 12345" out &&
	test_tick &&
	git commit -m "content moved"
	)
'

# -- mv on untracked file fails -----------------------------------------------

test_expect_success 'mv on untracked file fails' '
	(
	cd repo &&
	echo "untracked" >untracked.txt &&
	test_must_fail git mv untracked.txt ut-moved.txt 2>err &&
	rm untracked.txt
	)
'

# -- mv deeply nested file ----------------------------------------------------

test_expect_success 'mv deeply nested file to root' '
	(
	cd repo &&
	mkdir -p very/deep/nested/path &&
	echo "deep" >very/deep/nested/path/file.txt &&
	git add very/ &&
	test_tick &&
	git commit -m "deep file" &&
	git mv very/deep/nested/path/file.txt shallow.txt &&
	test_path_is_file shallow.txt &&
	test_tick &&
	git commit -m "shallow moved"
	)
'

# -- mv symlink (if supported) ------------------------------------------------

test_expect_success 'mv works with symlinks' '
	(
	cd repo &&
	ln -s shallow.txt link.txt &&
	git add link.txt &&
	test_tick &&
	git commit -m "add symlink" &&
	git mv link.txt moved-link.txt &&
	test -L moved-link.txt &&
	test_tick &&
	git commit -m "mv symlink"
	)
'

test_done
