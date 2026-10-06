#!/bin/sh
# Tests for git rm with directory trees and recursive removal.

test_description='rm directory tree and recursive removal'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- setup -------------------------------------------------------------------

test_expect_success 'setup: create repo with directory tree' '
	(
	git init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	mkdir -p a/b/c &&
	echo "root" >root.txt &&
	echo "a1" >a/a1.txt &&
	echo "a2" >a/a2.txt &&
	echo "b1" >a/b/b1.txt &&
	echo "b2" >a/b/b2.txt &&
	echo "c1" >a/b/c/c1.txt &&
	echo "c2" >a/b/c/c2.txt &&
	git add . &&
	test_tick &&
	git commit -m "initial tree"
	)
'

# -- recursive removal -------------------------------------------------------

test_expect_success 'rm without -r on directory fails' '
	(
	cd repo &&
	test_must_fail git rm a 2>err
	)
'

test_expect_success 'rm -r removes entire directory tree from index' '
	(
	cd repo &&
	git rm -r a &&
	git ls-files a/ >out &&
	test -z "$(cat out)"
	)
'

test_expect_success 'working tree directory is removed after rm -r' '
	(
	cd repo &&
	test_path_is_missing a
	)
'

test_expect_success 'commit after rm -r works' '
	(
	cd repo &&
	test_tick &&
	git commit -m "remove a" &&
	git ls-tree -r HEAD >out &&
	! grep "a/" out &&
	grep "root.txt" out
	)
'

# -- restore tree and test partial removal ------------------------------------

test_expect_success 'restore directory tree for partial tests' '
	(
	cd repo &&
	git checkout HEAD~1 -- a &&
	test_tick &&
	git commit -m "restore a/"
	)
'

test_expect_success 'rm -r on subdirectory removes only that subtree' '
	(
	cd repo &&
	git rm -r a/b/c &&
	git ls-files a/ >out &&
	grep "a/a1.txt" out &&
	grep "a/b/b1.txt" out &&
	! grep "a/b/c/" out
	)
'

test_expect_success 'parent directories with remaining files survive' '
	(
	cd repo &&
	test_path_is_dir a/b &&
	test_path_is_file a/a1.txt
	)
'

test_expect_success 'commit partial removal' '
	(
	cd repo &&
	test_tick &&
	git commit -m "remove a/b/c/"
	)
'

# -- rm --cached preserves working tree --------------------------------------

test_expect_success 'rm --cached removes from index but keeps files' '
	(
	cd repo &&
	git rm --cached a/b/b1.txt &&
	test_path_is_file a/b/b1.txt &&
	git ls-files a/b/b1.txt >out &&
	test -z "$(cat out)"
	)
'

test_expect_success 'rm --cached -r removes directory from index only' '
	(
	cd repo &&
	git add a/b/b1.txt &&
	git rm --cached -r a/b &&
	test_path_is_dir a/b &&
	git ls-files a/b/ >out &&
	test -z "$(cat out)"
	)
'

test_expect_success 'commit cached removal then re-add' '
	(
	cd repo &&
	test_tick &&
	git commit -m "cached rm a/b" &&
	git add a/b &&
	test_tick &&
	git commit -m "re-add a/b"
	)
'

# -- rm --dry-run -------------------------------------------------------------

test_expect_success 'rm --dry-run does not actually remove' '
	(
	cd repo &&
	git rm -r -n a >out 2>&1 &&
	git ls-files a/ >files &&
	test $(wc -l <files) -gt 0
	)
'

test_expect_success 'working tree intact after dry-run' '
	(
	cd repo &&
	test_path_is_dir a &&
	test_path_is_file a/a1.txt
	)
'

# -- rm --quiet ---------------------------------------------------------------

test_expect_success 'rm --quiet suppresses output' '
	(
	cd repo &&
	git rm -q a/a2.txt >out 2>&1 &&
	test -z "$(cat out)" &&
	test_tick &&
	git commit -m "quiet rm a2"
	)
'

# -- rm --force with local modifications --------------------------------------

test_expect_success 'rm refuses to remove file with local changes' '
	(
	cd repo &&
	echo "modified" >a/a1.txt &&
	git add a/a1.txt &&
	echo "more changes" >a/a1.txt &&
	test_must_fail git rm a/a1.txt 2>err
	)
'

test_expect_success 'rm -f removes file with local changes' '
	(
	cd repo &&
	git rm -f a/a1.txt &&
	test_path_is_missing a/a1.txt &&
	test_tick &&
	git commit -m "force rm a1"
	)
'

# -- rm --ignore-unmatch ------------------------------------------------------

test_expect_success 'rm on non-existent file fails' '
	(
	cd repo &&
	test_must_fail git rm nonexistent.txt 2>err
	)
'

test_expect_success 'rm --ignore-unmatch on non-existent file succeeds' '
	(
	cd repo &&
	git rm --ignore-unmatch nonexistent.txt
	)
'

# -- rm multiple explicit files -----------------------------------------------

test_expect_success 'setup for multi-file removal tests' '
	(
	cd repo &&
	mkdir -p multi &&
	echo "1" >multi/file1.log &&
	echo "2" >multi/file2.log &&
	echo "3" >multi/file3.txt &&
	git add multi/ &&
	test_tick &&
	git commit -m "multi files"
	)
'

test_expect_success 'rm multiple files by name' '
	(
	cd repo &&
	git rm multi/file1.log multi/file2.log &&
	git ls-files multi/ >out &&
	! grep "file1.log" out &&
	! grep "file2.log" out &&
	grep "file3.txt" out
	)
'

test_expect_success 'commit multi-file removal' '
	(
	cd repo &&
	test_tick &&
	git commit -m "rm multi logs"
	)
'

# -- rm on file in nested dirs ------------------------------------------------

test_expect_success 'rm file leaves parent dirs if other files remain' '
	(
	cd repo &&
	mkdir -p d/e/f &&
	echo "x" >d/e/f/x.txt &&
	echo "y" >d/e/y.txt &&
	git add d/ &&
	test_tick &&
	git commit -m "nested" &&
	git rm d/e/f/x.txt &&
	test_path_is_missing d/e/f &&
	test_path_is_dir d/e &&
	test_tick &&
	git commit -m "rm nested file"
	)
'

test_expect_success 'rm removes empty parent directories' '
	(
	cd repo &&
	git rm d/e/y.txt &&
	test_path_is_missing d &&
	test_tick &&
	git commit -m "rm last nested file"
	)
'

# -- rm on file only in index (not working tree) -----------------------------

test_expect_success 'rm --cached on file not in working tree succeeds' '
	(
	cd repo &&
	echo "phantom" >phantom.txt &&
	git add phantom.txt &&
	rm phantom.txt &&
	git rm --cached phantom.txt &&
	git ls-files phantom.txt >out &&
	test -z "$(cat out)"
	)
'

# -- rm -r on nested directory with single file --------------------------------

test_expect_success 'rm -r on dir with single file works' '
	(
	cd repo &&
	mkdir -p single/dir &&
	echo "only" >single/dir/only.txt &&
	git add single/ &&
	test_tick &&
	git commit -m "single file dir" &&
	git rm -r single &&
	test_path_is_missing single &&
	test_tick &&
	git commit -m "rm single"
	)
'

# -- rm after rename -----------------------------------------------------------

test_expect_success 'rm works on file that was mv-ed' '
	(
	cd repo &&
	echo "movable" >movable.txt &&
	git add movable.txt &&
	test_tick &&
	git commit -m "add movable" &&
	git mv movable.txt moved.txt &&
	git rm moved.txt &&
	git ls-files moved.txt >out &&
	test -z "$(cat out)" &&
	git checkout -- movable.txt 2>/dev/null;
	git reset HEAD 2>/dev/null;
	true
	)
'

test_done
