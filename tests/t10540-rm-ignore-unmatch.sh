#!/bin/sh
# Tests for grit rm with --ignore-unmatch, --cached, -f, -r, -n, -q flags
# and various edge cases around removing tracked/untracked files.

test_description='grit rm --ignore-unmatch and related flags'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with files' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@example.com" &&
	git config user.name "Test User" &&
	echo "hello" >file1.txt &&
	echo "world" >file2.txt &&
	echo "foo" >file3.txt &&
	mkdir -p sub/deep &&
	echo "nested" >sub/nested.txt &&
	echo "deeper" >sub/deep/deep.txt &&
	echo "readme" >README.md &&
	grit add . &&
	grit commit -m "initial commit"
	)
'

# --- --ignore-unmatch ---

test_expect_success 'rm nonexistent file fails without --ignore-unmatch' '
	(
	cd repo &&
	test_must_fail grit rm nonexistent.txt 2>err &&
	test -s err
	)
'

test_expect_success 'rm nonexistent file succeeds with --ignore-unmatch' '
	(
	cd repo &&
	grit rm --ignore-unmatch nonexistent.txt
	)
'

test_expect_success 'rm --ignore-unmatch exit code is zero' '
	(
	cd repo &&
	grit rm --ignore-unmatch no-such-file.txt &&
	test $? -eq 0
	)
'

test_expect_success 'rm --ignore-unmatch with glob matching nothing succeeds' '
	(
	cd repo &&
	grit rm --ignore-unmatch "*.xyz"
	)
'

test_expect_success 'rm --ignore-unmatch does not remove existing files when mixed with nonexistent' '
	(
	cd repo &&
	grit rm --ignore-unmatch nonexistent.txt &&
	test -f file1.txt
	)
'

test_expect_success 'rm existing file works normally even with --ignore-unmatch' '
	(
	cd repo &&
	echo "removeme" >to_remove.txt &&
	grit add to_remove.txt &&
	grit commit -m "add to_remove" &&
	grit rm --ignore-unmatch to_remove.txt &&
	! test -f to_remove.txt &&
	grit status >status_out &&
	grep "to_remove.txt" status_out
	)
'

# --- --cached ---

test_expect_success 'rm --cached removes from index but keeps working tree' '
	(
	cd repo &&
	echo "cached" >cached_file.txt &&
	grit add cached_file.txt &&
	grit commit -m "add cached_file" &&
	grit rm --cached cached_file.txt &&
	test -f cached_file.txt &&
	grit ls-files >ls_out &&
	! grep "cached_file.txt" ls_out
	)
'

test_expect_success 'rm --cached with multiple files' '
	(
	cd repo &&
	echo "a" >aa.txt &&
	echo "b" >bb.txt &&
	grit add aa.txt bb.txt &&
	grit commit -m "add aa bb" &&
	grit rm --cached aa.txt bb.txt &&
	test -f aa.txt &&
	test -f bb.txt &&
	grit ls-files >ls_out &&
	! grep "aa.txt" ls_out &&
	! grep "bb.txt" ls_out
	)
'

test_expect_success 'rm --cached --ignore-unmatch on nonexistent file' '
	(
	cd repo &&
	grit rm --cached --ignore-unmatch nofile.txt
	)
'

# --- -f (force) ---

test_expect_success 'rm -f removes file with local modifications' '
	(
	cd repo &&
	echo "original" >force_file.txt &&
	grit add force_file.txt &&
	grit commit -m "add force_file" &&
	echo "modified locally" >force_file.txt &&
	grit rm -f force_file.txt &&
	! test -f force_file.txt
	)
'

test_expect_success 'rm without -f on modified file fails' '
	(
	cd repo &&
	echo "content" >mod_file.txt &&
	grit add mod_file.txt &&
	grit commit -m "add mod_file" &&
	echo "changed" >mod_file.txt &&
	grit add mod_file.txt &&
	echo "changed again" >mod_file.txt &&
	test_must_fail grit rm mod_file.txt 2>err
	)
'

# --- -r (recursive) ---

test_expect_success 'rm -r removes directory contents' '
	(
	cd repo &&
	mkdir -p rmdir/inner &&
	echo "x" >rmdir/x.txt &&
	echo "y" >rmdir/inner/y.txt &&
	grit add rmdir &&
	grit commit -m "add rmdir" &&
	grit rm -r rmdir &&
	! test -d rmdir &&
	grit ls-files >ls_out &&
	! grep "rmdir" ls_out
	)
'

test_expect_success 'rm without -r on directory fails' '
	(
	cd repo &&
	mkdir -p norecdir &&
	echo "z" >norecdir/z.txt &&
	grit add norecdir &&
	grit commit -m "add norecdir" &&
	test_must_fail grit rm norecdir 2>err
	)
'

test_expect_success 'rm -r on nested directories' '
	(
	cd repo &&
	mkdir -p a/b/c &&
	echo "1" >a/1.txt &&
	echo "2" >a/b/2.txt &&
	echo "3" >a/b/c/3.txt &&
	grit add a &&
	grit commit -m "add nested" &&
	grit rm -r a &&
	! test -d a &&
	grit ls-files >ls_out &&
	! grep "^a/" ls_out
	)
'

# --- -n / --dry-run ---

test_expect_success 'rm --dry-run shows what would be removed without removing' '
	(
	cd repo &&
	echo "dry" >dry_file.txt &&
	grit add dry_file.txt &&
	grit commit -m "add dry_file" &&
	grit rm -n dry_file.txt >dry_out 2>&1 &&
	test -f dry_file.txt &&
	grit ls-files >ls_out &&
	grep "dry_file.txt" ls_out
	)
'

test_expect_success 'rm --dry-run output mentions file' '
	(
	cd repo &&
	grit rm --dry-run dry_file.txt >dry_out 2>&1 &&
	grep "dry_file.txt" dry_out
	)
'

test_expect_success 'rm -n with multiple files does not remove any' '
	(
	cd repo &&
	echo "one" >dry1.txt &&
	echo "two" >dry2.txt &&
	grit add dry1.txt dry2.txt &&
	grit commit -m "add dry1 dry2" &&
	grit rm -n dry1.txt dry2.txt >dry_out 2>&1 &&
	test -f dry1.txt &&
	test -f dry2.txt
	)
'

# --- -q / --quiet ---

test_expect_success 'rm -q suppresses normal output' '
	(
	cd repo &&
	echo "quiet" >quiet_file.txt &&
	grit add quiet_file.txt &&
	grit commit -m "add quiet_file" &&
	grit rm -q quiet_file.txt >quiet_out 2>&1 &&
	test_must_be_empty quiet_out
	)
'

test_expect_success 'rm without -q shows output' '
	(
	cd repo &&
	echo "loud" >loud_file.txt &&
	grit add loud_file.txt &&
	grit commit -m "add loud_file" &&
	grit rm loud_file.txt >loud_out 2>&1 &&
	test -s loud_out
	)
'

# --- Combined flags ---

test_expect_success 'rm --cached --ignore-unmatch combined' '
	(
	cd repo &&
	echo "combo" >combo.txt &&
	grit add combo.txt &&
	grit commit -m "add combo" &&
	grit rm --cached --ignore-unmatch combo.txt &&
	test -f combo.txt &&
	grit ls-files >ls_out &&
	! grep "combo.txt" ls_out
	)
'

test_expect_success 'rm -r --dry-run on directory does not remove' '
	(
	cd repo &&
	mkdir -p drydir &&
	echo "p" >drydir/p.txt &&
	grit add drydir &&
	grit commit -m "add drydir" &&
	grit rm -r --dry-run drydir >dry_out 2>&1 &&
	test -d drydir &&
	test -f drydir/p.txt
	)
'

test_expect_success 'rm -r -q removes silently' '
	(
	cd repo &&
	mkdir -p silentdir &&
	echo "s" >silentdir/s.txt &&
	grit add silentdir &&
	grit commit -m "add silentdir" &&
	grit rm -r -q silentdir >q_out 2>&1 &&
	test_must_be_empty q_out &&
	! test -d silentdir
	)
'

test_expect_success 'rm -r -f removes directory with modifications' '
	(
	cd repo &&
	mkdir -p forcedir &&
	echo "orig" >forcedir/orig.txt &&
	grit add forcedir &&
	grit commit -m "add forcedir" &&
	echo "modified" >forcedir/orig.txt &&
	grit rm -r -f forcedir &&
	! test -d forcedir
	)
'

test_expect_success 'rm --ignore-unmatch --dry-run on nonexistent file' '
	(
	cd repo &&
	grit rm --ignore-unmatch --dry-run ghost.txt
	)
'

# --- Edge cases ---

test_expect_success 'rm file then restore via reset' '
	(
	cd repo &&
	echo "comeback" >comeback.txt &&
	grit add comeback.txt &&
	grit commit -m "add comeback" &&
	grit rm comeback.txt &&
	! test -f comeback.txt &&
	grit reset HEAD -- comeback.txt &&
	grit checkout -- comeback.txt &&
	test -f comeback.txt
	)
'

test_expect_success 'rm all files in directory leaves empty dir on filesystem with --cached' '
	(
	cd repo &&
	mkdir -p keepdir &&
	echo "k" >keepdir/k.txt &&
	grit add keepdir &&
	grit commit -m "add keepdir" &&
	grit rm --cached keepdir/k.txt &&
	test -d keepdir &&
	test -f keepdir/k.txt
	)
'

test_expect_success 'rm file with spaces in name' '
	(
	cd repo &&
	echo "spaced" >"file with spaces.txt" &&
	grit add "file with spaces.txt" &&
	grit commit -m "add spaced" &&
	grit rm "file with spaces.txt" &&
	! test -f "file with spaces.txt"
	)
'

test_expect_success 'rm file that is already deleted from worktree' '
	(
	cd repo &&
	echo "gone" >will_delete.txt &&
	grit add will_delete.txt &&
	grit commit -m "add will_delete" &&
	rm will_delete.txt &&
	grit rm will_delete.txt &&
	grit ls-files >ls_out &&
	! grep "will_delete.txt" ls_out
	)
'

test_expect_success 'rm multiple specific files at once' '
	(
	cd repo &&
	echo "m1" >multi1.txt &&
	echo "m2" >multi2.txt &&
	echo "m3" >multi3.txt &&
	grit add multi1.txt multi2.txt multi3.txt &&
	grit commit -m "add multi" &&
	grit rm multi1.txt multi2.txt multi3.txt &&
	! test -f multi1.txt &&
	! test -f multi2.txt &&
	! test -f multi3.txt
	)
'

test_expect_success 'rm --cached on staged-only file (never committed)' '
	(
	cd repo &&
	echo "staged" >staged_only.txt &&
	grit add staged_only.txt &&
	grit rm --cached staged_only.txt &&
	test -f staged_only.txt &&
	grit ls-files >ls_out &&
	! grep "staged_only.txt" ls_out
	)
'

test_done
