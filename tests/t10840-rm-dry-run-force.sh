#!/bin/sh
# Tests for grit rm with --dry-run (-n) and --force (-f) flags,
# including combinations with --cached, -r, -q, and edge cases.

test_description='grit rm --dry-run and --force interactions'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@example.com" &&
	git config user.name "Test User" &&
	echo "aaa" >a.txt &&
	echo "bbb" >b.txt &&
	echo "ccc" >c.txt &&
	mkdir -p dir/sub &&
	echo "d1" >dir/d1.txt &&
	echo "d2" >dir/sub/d2.txt &&
	echo "keep" >keep.txt &&
	grit add . &&
	grit commit -m "initial"
	)
'

# --- dry-run basics ---

test_expect_success 'rm -n does not remove file from worktree' '
	(
	cd repo &&
	grit rm -n a.txt &&
	test -f a.txt
	)
'

test_expect_success 'rm --dry-run does not remove file from worktree' '
	(
	cd repo &&
	grit rm --dry-run b.txt &&
	test -f b.txt
	)
'

test_expect_success 'rm -n does not remove file from index' '
	(
	cd repo &&
	grit rm -n a.txt &&
	grit ls-files >ls_out &&
	grep "a.txt" ls_out
	)
'

test_expect_success 'rm --dry-run prints what would be removed' '
	(
	cd repo &&
	grit rm --dry-run a.txt >out 2>&1 &&
	grep "a.txt" out
	)
'

test_expect_success 'rm -n with multiple files shows all' '
	(
	cd repo &&
	grit rm -n a.txt b.txt c.txt >out 2>&1 &&
	grep "a.txt" out &&
	grep "b.txt" out &&
	grep "c.txt" out
	)
'

test_expect_success 'rm -n -r on directory does not remove anything' '
	(
	cd repo &&
	grit rm -n -r dir/ &&
	test -f dir/d1.txt &&
	test -f dir/sub/d2.txt &&
	grit ls-files >ls_out &&
	grep "dir/d1.txt" ls_out
	)
'

test_expect_success 'rm --dry-run combined with --cached does not unstage' '
	(
	cd repo &&
	grit rm --dry-run --cached a.txt &&
	grit ls-files >ls_out &&
	grep "a.txt" ls_out
	)
'

test_expect_success 'rm -n exit code is zero for tracked file' '
	(
	cd repo &&
	grit rm -n a.txt
	)
'

test_expect_success 'rm -n on nonexistent file fails' '
	(
	cd repo &&
	test_must_fail grit rm -n nonexistent.txt
	)
'

test_expect_success 'rm -n --ignore-unmatch on nonexistent file succeeds' '
	(
	cd repo &&
	grit rm -n --ignore-unmatch nonexistent.txt
	)
'

# --- force basics ---

test_expect_success 'setup modified tracked file for force tests' '
	(
	cd repo &&
	echo "modified" >a.txt &&
	grit add a.txt &&
	echo "locally changed" >a.txt
	)
'

test_expect_success 'rm without -f fails when file has local changes' '
	(
	cd repo &&
	test_must_fail grit rm a.txt 2>err &&
	test -f a.txt
	)
'

test_expect_success 'rm -f removes file with local changes' '
	(
	cd repo &&
	grit rm -f a.txt &&
	! test -f a.txt
	)
'

test_expect_success 'rm -f actually removes from index' '
	(
	cd repo &&
	grit ls-files >ls_out &&
	! grep "^a.txt$" ls_out
	)
'

test_expect_success 'restore a.txt for more tests' '
	(
	cd repo &&
	grit reset HEAD -- a.txt &&
	grit checkout -- a.txt 2>/dev/null || grit restore a.txt
	)
'

test_expect_success 'rm --force removes staged-only changes' '
	(
	cd repo &&
	echo "staged change" >b.txt &&
	grit add b.txt &&
	grit rm --force b.txt &&
	! test -f b.txt &&
	grit ls-files >ls_out &&
	! grep "^b.txt$" ls_out
	)
'

test_expect_success 'restore b.txt' '
	(
	cd repo &&
	grit reset HEAD -- b.txt &&
	grit checkout -- b.txt 2>/dev/null || grit restore b.txt
	)
'

# --- dry-run + force combinations ---

test_expect_success 'rm -n -f on modified file shows but does not remove' '
	(
	cd repo &&
	echo "dirty" >c.txt &&
	grit rm -n -f c.txt >out 2>&1 &&
	test -f c.txt &&
	grit ls-files >ls_out &&
	grep "c.txt" ls_out
	)
'

test_expect_success 'clean up c.txt modifications' '
	(
	cd repo &&
	grit checkout -- c.txt 2>/dev/null || grit restore c.txt
	)
'

test_expect_success 'rm --dry-run --force on clean file shows removal' '
	(
	cd repo &&
	grit rm --dry-run --force keep.txt >out 2>&1 &&
	test -f keep.txt &&
	grep "keep.txt" out
	)
'

# --- force with recursive ---

test_expect_success 'rm -f -r removes directory with modified files' '
	(
	cd repo &&
	echo "dirty-d1" >dir/d1.txt &&
	grit rm -f -r dir/ &&
	! test -d dir
	)
'

test_expect_success 'index no longer has dir/ entries after rm -f -r' '
	(
	cd repo &&
	grit ls-files >ls_out &&
	! grep "^dir/" ls_out
	)
'

# --- force with cached ---

test_expect_success 'rm --cached does not need -f even with local changes' '
	(
	cd repo &&
	echo "changed-locally" >a.txt &&
	grit rm --cached a.txt &&
	test -f a.txt &&
	grit ls-files >ls_out &&
	! grep "^a.txt$" ls_out
	)
'

test_expect_success 'restore a.txt to index and clean state' '
	(
	cd repo &&
	grit add a.txt &&
	grit commit -m "re-add a.txt"
	)
'

# --- quiet + dry-run ---

test_expect_success 'rm -q -n keeps file intact' '
	(
	cd repo &&
	grit rm -q -n a.txt &&
	test -f a.txt
	)
'

test_expect_success 'rm -q removes file silently' '
	(
	cd repo &&
	echo "qqq" >quiet_rm.txt &&
	grit add quiet_rm.txt &&
	grit commit -m "add quiet_rm" &&
	grit rm -q quiet_rm.txt >out 2>&1 &&
	! test -f quiet_rm.txt &&
	test ! -s out
	)
'

# --- edge cases ---

test_expect_success 'rm -n on file only in index (not committed) shows it' '
	(
	cd repo &&
	echo "new-only" >index_only.txt &&
	grit add index_only.txt &&
	grit rm -n --cached index_only.txt >out 2>&1 &&
	grit ls-files >ls_out &&
	grep "index_only.txt" ls_out
	)
'

test_expect_success 'rm -f on file only in index removes it' '
	(
	cd repo &&
	grit rm -f index_only.txt &&
	! test -f index_only.txt &&
	grit ls-files >ls_out &&
	! grep "index_only.txt" ls_out
	)
'

test_expect_success 'rm --dry-run does not affect subsequent real rm' '
	(
	cd repo &&
	grit rm -n c.txt &&
	test -f c.txt &&
	grit rm c.txt &&
	! test -f c.txt
	)
'

test_expect_success 'rm -f with --ignore-unmatch on nonexistent file' '
	(
	cd repo &&
	grit rm -f --ignore-unmatch totally-absent.txt
	)
'

test_expect_success 'rm without -r on directory fails' '
	(
	cd repo &&
	mkdir -p dir2/sub &&
	echo "x" >dir2/sub/x.txt &&
	grit add dir2/ &&
	grit commit -m "add dir2" &&
	test_must_fail grit rm dir2/sub 2>err
	)
'

test_expect_success 'rm -n -r --cached on directory does not unstage' '
	(
	cd repo &&
	grit rm -n -r --cached dir2/ &&
	grit ls-files >ls_out &&
	grep "dir2/" ls_out
	)
'

test_done
