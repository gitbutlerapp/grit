#!/bin/sh
#
# Tests for status with untracked files and directories
#

test_description='status with untracked files and directories'

. ./test-lib.sh

# Use a temp dir outside the repo for output files to avoid polluting status
TOUTDIR="$TEST_DIRECTORY/trash-output"

test_expect_success 'setup: init repo with config' '
	mkdir -p "$TOUTDIR" &&
	git init &&
	git config user.email "test@test.com" &&
	git config user.name "Test User" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	if test -e .bin
	then
		git add .bin &&
		git commit -m "add bin wrappers"
	else
		git commit --allow-empty -m "setup"
	fi
'

test_expect_success 'setup: create initial commit' '
	echo "tracked" >tracked.txt &&
	git add tracked.txt &&
	git commit -m "initial"
'

test_expect_success 'status on clean tree shows nothing' '
	rm -f "$TOUTDIR/out" &&
	git status -s >"$TOUTDIR/out" &&
	test_must_be_empty "$TOUTDIR/out"
'

test_expect_success 'status shows untracked file' '
	echo "untracked" >untracked.txt &&
	git status -s >"$TOUTDIR/out" &&
	grep -q "??" "$TOUTDIR/out" &&
	grep -q "untracked.txt" "$TOUTDIR/out"
'

test_expect_success 'status shows untracked directory' '
	mkdir -p newdir &&
	echo "file" >newdir/file.txt &&
	git status -s >"$TOUTDIR/out" &&
	grep -q "newdir" "$TOUTDIR/out"
'

test_expect_success 'status --short format is correct' '
	git status -s >"$TOUTDIR/out" &&
	grep -q "^??" "$TOUTDIR/out"
'

test_expect_success 'status porcelain shows untracked' '
	git status --porcelain >"$TOUTDIR/out" &&
	grep -q "??" "$TOUTDIR/out"
'

test_expect_success 'status shows modified tracked file' '
	echo "modified" >tracked.txt &&
	git status -s >"$TOUTDIR/out" &&
	grep -q "M" "$TOUTDIR/out" &&
	grep -q "tracked.txt" "$TOUTDIR/out"
'

test_expect_success 'status shows staged file' '
	git add tracked.txt &&
	git status -s >"$TOUTDIR/out" &&
	grep -q "M" "$TOUTDIR/out"
'

test_expect_success 'commit and clean up' '
	git add . &&
	git commit -m "add everything"
'

test_expect_success 'status on clean tree again' '
	git status -s >"$TOUTDIR/out" &&
	test_must_be_empty "$TOUTDIR/out"
'

test_expect_success 'status with nested untracked dirs' '
	mkdir -p a/b/c &&
	echo "deep" >a/b/c/file.txt &&
	git status -s >"$TOUTDIR/out" &&
	grep -q "a/" "$TOUTDIR/out"
'

test_expect_success 'status with multiple untracked dirs' '
	mkdir -p dir1 dir2 &&
	echo "f1" >dir1/f.txt &&
	echo "f2" >dir2/f.txt &&
	git status -s >"$TOUTDIR/out" &&
	grep -q "dir1" "$TOUTDIR/out" &&
	grep -q "dir2" "$TOUTDIR/out"
'

test_expect_success 'status with untracked file in tracked dir' '
	echo "new" >newdir/new.txt &&
	git status -s >"$TOUTDIR/out" &&
	grep -q "newdir/new.txt" "$TOUTDIR/out" || grep -q "newdir" "$TOUTDIR/out"
'

test_expect_success 'add and commit all untracked' '
	git add . &&
	git commit -m "add all dirs"
'

test_expect_success 'status -u no suppresses untracked' '
	echo "ut" >ut.txt &&
	git status -u no -s >"$TOUTDIR/out" &&
	! grep -q "ut.txt" "$TOUTDIR/out"
'

test_expect_success 'status -u normal shows untracked files' '
	git status -u normal -s >"$TOUTDIR/out" &&
	grep -q "ut.txt" "$TOUTDIR/out"
'

test_expect_success 'status with untracked dir' '
	mkdir -p udir &&
	echo "uf" >udir/uf.txt &&
	git status -s >"$TOUTDIR/out" &&
	grep -q "udir" "$TOUTDIR/out"
'

test_expect_success 'clean up untracked' '
	rm -rf ut.txt udir &&
	git status -s >"$TOUTDIR/out" &&
	test_must_be_empty "$TOUTDIR/out"
'

test_expect_success 'status with deleted file' '
	rm tracked.txt &&
	git status -s >"$TOUTDIR/out" &&
	grep -q "D" "$TOUTDIR/out" &&
	grep -q "tracked.txt" "$TOUTDIR/out"
'

test_expect_success 'status with staged deletion' '
	git add tracked.txt &&
	git status -s >"$TOUTDIR/out" &&
	grep -q "D" "$TOUTDIR/out"
'

test_expect_success 'commit deletion' '
	git commit -m "delete tracked"
'

test_expect_success 'status --branch shows branch info' '
	git status -s -b >"$TOUTDIR/out" &&
	grep -q "master" "$TOUTDIR/out" || grep -q "main" "$TOUTDIR/out"
'

test_expect_success 'status with .gitignore shows it as untracked' '
	echo "*.log" >.gitignore &&
	git status -s >"$TOUTDIR/out" &&
	grep -q ".gitignore" "$TOUTDIR/out"
'

test_expect_success 'status --ignored shows ignored files' '
	echo "log data" >test.log &&
	git status --ignored -s >"$TOUTDIR/out" &&
	grep -q "test.log" "$TOUTDIR/out"
'

test_expect_success 'add gitignore and commit' '
	rm -f test.log &&
	git add .gitignore &&
	git commit -m "add gitignore"
'

test_expect_success 'status does not show empty untracked directory' '
	mkdir -p emptydir &&
	rm -f "$TOUTDIR/out" &&
	git status -s >"$TOUTDIR/out" &&
	! grep -q "emptydir" "$TOUTDIR/out"
'

test_expect_success 'status with file inside directory' '
	echo "f" >emptydir/f.txt &&
	git status -s >"$TOUTDIR/out" &&
	grep -q "emptydir" "$TOUTDIR/out"
'

test_expect_success 'status porcelain format is stable' '
	git status --porcelain >"$TOUTDIR/out" &&
	grep -q "^??" "$TOUTDIR/out"
'

test_expect_success 'commit everything' '
	rm -f test.log &&
	git add . &&
	git commit -m "add stuff"
'

test_expect_success 'status with renamed file (stage rm+add)' '
	echo "rename test" >newdir/orig.txt &&
	git add newdir/orig.txt &&
	git commit -m "add orig" &&
	git rm newdir/orig.txt &&
	echo "rename test" >newdir/renamed.txt &&
	git add newdir/renamed.txt &&
	git status -s >"$TOUTDIR/out" &&
	test -s "$TOUTDIR/out"
'

test_expect_success 'commit rename' '
	git commit -m "rename"
'

test_expect_success 'status with binary untracked file' '
	printf "\000\001\002" >binary.dat &&
	git status -s >"$TOUTDIR/out" &&
	grep -q "binary.dat" "$TOUTDIR/out"
'

test_expect_success 'status with symlink' '
	ln -s newdir/new.txt link.txt &&
	git status -s >"$TOUTDIR/out" &&
	grep -q "link.txt" "$TOUTDIR/out"
'

test_expect_success 'add and commit symlink and binary' '
	git add . &&
	git commit -m "add binary and symlink"
'

test_expect_success 'status clean after final commit' '
	git status -s >"$TOUTDIR/out" &&
	test_must_be_empty "$TOUTDIR/out"
'

test_expect_success 'cleanup temp dir' '
	rm -rf "$TOUTDIR"
'

test_done
