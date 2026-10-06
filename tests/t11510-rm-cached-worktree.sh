#!/bin/sh
# Tests for grit rm: --cached, -r, -f, --dry-run, --quiet, --ignore-unmatch,
# and worktree interactions.

test_description='grit rm: cached, recursive, force, dry-run, quiet, ignore-unmatch'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: init repo with files' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "a" >file_a.txt &&
	echo "b" >file_b.txt &&
	echo "c" >file_c.txt &&
	grit add file_a.txt file_b.txt file_c.txt &&
	grit commit -m "initial"
	)
'

# ---- basic rm ----
test_expect_success 'rm removes file from index and worktree' '
	(
	cd repo &&
	grit rm file_a.txt &&
	! test -f file_a.txt &&
	! grit ls-files --error-unmatch file_a.txt 2>/dev/null
	)
'

test_expect_success 'rm shows removal message' '
	(
	cd repo &&
	echo "x" >torm.txt &&
	grit add torm.txt &&
	grit commit -m "add torm" &&
	grit rm torm.txt >output 2>&1 &&
	grep "torm.txt" output
	)
'

# ---- rm --cached ----
test_expect_success 'rm --cached removes from index but keeps worktree file' '
	(
	cd repo &&
	grit rm --cached file_b.txt &&
	test -f file_b.txt &&
	! grit ls-files --error-unmatch file_b.txt 2>/dev/null
	)
'

test_expect_success 'rm --cached file shows as untracked in status' '
	(
	cd repo &&
	grit status --porcelain >st &&
	grep "??" st | grep "file_b.txt"
	)
'

test_expect_success 're-add after rm --cached works' '
	(
	cd repo &&
	grit add file_b.txt &&
	grit ls-files --error-unmatch file_b.txt
	)
'

# ---- recursive rm ----
test_expect_success 'setup directory structure for recursive rm' '
	(
	cd repo &&
	mkdir -p dir/sub &&
	echo "d1" >dir/d1.txt &&
	echo "d2" >dir/sub/d2.txt &&
	grit add dir &&
	grit commit -m "add dir"
	)
'

test_expect_success 'rm -r removes directory recursively from index' '
	(
	cd repo &&
	grit rm -r dir &&
	! grit ls-files --error-unmatch dir/d1.txt 2>/dev/null &&
	! grit ls-files --error-unmatch dir/sub/d2.txt 2>/dev/null
	)
'

test_expect_success 'rm -r removes files from worktree' '
	(
	cd repo &&
	! test -f dir/d1.txt &&
	! test -f dir/sub/d2.txt
	)
'

test_expect_success 'rm -r --cached keeps worktree files' '
	(
	cd repo &&
	mkdir -p dir2/sub &&
	echo "x" >dir2/x.txt &&
	echo "y" >dir2/sub/y.txt &&
	grit add dir2 &&
	grit commit -m "add dir2" &&
	grit rm -r --cached dir2 &&
	test -f dir2/x.txt &&
	test -f dir2/sub/y.txt &&
	! grit ls-files --error-unmatch dir2/x.txt 2>/dev/null
	)
'

# ---- force ----
test_expect_success 'rm refuses to remove locally modified file' '
	(
	cd repo &&
	echo "orig" >modified.txt &&
	grit add modified.txt &&
	grit commit -m "add modified" &&
	echo "changed" >modified.txt &&
	test_must_fail grit rm modified.txt 2>err
	)
'

test_expect_success 'rm -f forces removal of modified file' '
	(
	cd repo &&
	grit rm -f modified.txt &&
	! test -f modified.txt &&
	! grit ls-files --error-unmatch modified.txt 2>/dev/null
	)
'

test_expect_success 'rm -f removes staged changes' '
	(
	cd repo &&
	echo "staged" >staged.txt &&
	grit add staged.txt &&
	echo "changed" >staged.txt &&
	grit rm -f staged.txt &&
	! test -f staged.txt
	)
'

# ---- dry-run ----
test_expect_success 'rm --dry-run does not remove file' '
	(
	cd repo &&
	echo "keep" >keep.txt &&
	grit add keep.txt &&
	grit commit -m "add keep" &&
	grit rm --dry-run keep.txt &&
	test -f keep.txt &&
	grit ls-files --error-unmatch keep.txt
	)
'

test_expect_success 'rm -n is synonym for --dry-run' '
	(
	cd repo &&
	grit rm -n keep.txt &&
	test -f keep.txt &&
	grit ls-files --error-unmatch keep.txt
	)
'

# ---- quiet ----
test_expect_success 'rm --quiet suppresses output' '
	(
	cd repo &&
	echo "q" >quiet.txt &&
	grit add quiet.txt &&
	grit commit -m "quiet" &&
	grit rm --quiet quiet.txt >output 2>&1 &&
	test ! -s output
	)
'

test_expect_success 'rm -q is synonym for --quiet' '
	(
	cd repo &&
	echo "q2" >quiet2.txt &&
	grit add quiet2.txt &&
	grit commit -m "quiet2" &&
	grit rm -q quiet2.txt >output 2>&1 &&
	test ! -s output
	)
'

# ---- ignore-unmatch ----
test_expect_success 'rm fails on nonexistent file' '
	(
	cd repo &&
	test_must_fail grit rm no-such-file 2>err
	)
'

test_expect_success 'rm --ignore-unmatch succeeds on nonexistent file' '
	(
	cd repo &&
	grit rm --ignore-unmatch no-such-file
	)
'

# ---- multiple files ----
test_expect_success 'rm multiple files at once' '
	(
	cd repo &&
	echo "m1" >multi1.txt &&
	echo "m2" >multi2.txt &&
	grit add multi1.txt multi2.txt &&
	grit commit -m "multi" &&
	grit rm multi1.txt multi2.txt &&
	! grit ls-files --error-unmatch multi1.txt 2>/dev/null &&
	! grit ls-files --error-unmatch multi2.txt 2>/dev/null
	)
'

# ---- rm after add but before commit ----
test_expect_success 'rm --cached on staged-but-uncommitted file' '
	(
	cd repo &&
	echo "new" >new.txt &&
	grit add new.txt &&
	grit rm --cached new.txt &&
	! grit ls-files --error-unmatch new.txt 2>/dev/null &&
	test -f new.txt
	)
'

# ---- rm in subdirectory ----
test_expect_success 'rm works from subdirectory with full path' '
	(
	cd repo &&
	mkdir -p subdir &&
	echo "sub" >subdir/sub.txt &&
	grit add subdir/sub.txt &&
	grit commit -m "subdir" &&
	grit rm subdir/sub.txt &&
	! grit ls-files --error-unmatch subdir/sub.txt 2>/dev/null
	)
'

# ---- rm --cached on intent-to-add ----
test_expect_success 'rm --cached removes intent-to-add entry' '
	(
	cd repo &&
	echo "ita" >ita.txt &&
	grit add -N ita.txt &&
	grit rm --cached ita.txt &&
	! grit ls-files --error-unmatch ita.txt 2>/dev/null &&
	test -f ita.txt
	)
'

# ---- rm then status ----
test_expect_success 'rm shows deletion in status' '
	(
	cd repo &&
	grit add file_c.txt 2>/dev/null &&
	grit commit -m "ensure fc" --allow-empty &&
	grit ls-files --error-unmatch file_c.txt &&
	grit rm file_c.txt &&
	grit status --porcelain >st &&
	grep "D  file_c.txt" st
	)
'

# ---- rm then commit ----
test_expect_success 'commit after rm records deletion' '
	(
	cd repo &&
	grit commit -m "delete file_c" &&
	grit log --oneline | grep "delete file_c" &&
	! grit ls-tree HEAD -- file_c.txt | grep file_c
	)
'

# ---- rm with -C flag ----
test_expect_success 'rm -C changes directory context' '
	(
	cd repo &&
	mkdir -p cdir &&
	echo "cf" >cdir/cf.txt &&
	grit add cdir/cf.txt &&
	grit commit -m "add cdir" &&
	cd .. &&
	grit -C repo rm cdir/cf.txt &&
	cd repo &&
	! grit ls-files --error-unmatch cdir/cf.txt 2>/dev/null
	)
'

# ---- deeply nested rm -r ----
test_expect_success 'rm -r handles deeply nested directories' '
	(
	cd repo &&
	mkdir -p a/b/c/d &&
	echo "deep" >a/b/c/d/deep.txt &&
	grit add a &&
	grit commit -m "deep" &&
	grit rm -r a &&
	! grit ls-files --error-unmatch a/b/c/d/deep.txt 2>/dev/null &&
	! test -f a/b/c/d/deep.txt
	)
'

# ---- rm file that does not exist on disk but is tracked ----
test_expect_success 'rm succeeds when worktree file already deleted' '
	(
	cd repo &&
	echo "ghost" >ghost.txt &&
	grit add ghost.txt &&
	grit commit -m "ghost" &&
	rm ghost.txt &&
	grit rm ghost.txt &&
	! grit ls-files --error-unmatch ghost.txt 2>/dev/null
	)
'

# ---- rm --cached on committed file ----
test_expect_success 'rm --cached then re-add and commit roundtrip' '
	(
	cd repo &&
	echo "roundtrip" >roundtrip.txt &&
	grit add roundtrip.txt &&
	grit commit -m "roundtrip" &&
	grit rm --cached roundtrip.txt &&
	grit add roundtrip.txt &&
	grit ls-files --error-unmatch roundtrip.txt
	)
'

test_expect_success 'rm --cached on committed file keeps it in worktree' '
	(
	cd repo &&
	echo "committed" >committed.txt &&
	grit add committed.txt &&
	grit commit -m "committed" &&
	grit rm --cached committed.txt &&
	test -f committed.txt &&
	! grit ls-files --error-unmatch committed.txt 2>/dev/null
	)
'

test_done
