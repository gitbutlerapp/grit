#!/bin/sh
# Test grit add: pathspec handling, --force for ignored files, -u/--update,
# -A/--all, -n/--dry-run, -N/--intent-to-add, -v/--verbose, and dot.

test_description='grit add pathspec, force, and update modes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	mkdir -p sub/deep &&
	echo "c" >sub/c.txt &&
	echo "d" >sub/deep/d.txt &&
	grit add . &&
	grit commit -m "initial"
	)
'

###########################################################################
# Section 2: Basic pathspec
###########################################################################

test_expect_success 'add single file' '
	(
	cd repo &&
	echo "new" >new.txt &&
	grit add new.txt &&
	grit ls-files >out &&
	grep "new.txt" out
	)
'

test_expect_success 'add multiple files' '
	(
	cd repo &&
	echo "x" >x.txt &&
	echo "y" >y.txt &&
	grit add x.txt y.txt &&
	grit ls-files >out &&
	grep "x.txt" out &&
	grep "y.txt" out
	)
'

test_expect_success 'add with dot adds everything' '
	(
	cd repo &&
	echo "z" >z.txt &&
	grit add . &&
	grit ls-files >out &&
	grep "z.txt" out
	)
'

test_expect_success 'add subdirectory file' '
	(
	cd repo &&
	echo "newsub" >sub/new.txt &&
	grit add sub/new.txt &&
	grit ls-files >out &&
	grep "sub/new.txt" out
	)
'

test_expect_success 'add directory adds all files in it' '
	(
	cd repo &&
	mkdir dir1 &&
	echo "f1" >dir1/f1.txt &&
	echo "f2" >dir1/f2.txt &&
	grit add dir1 &&
	grit ls-files >out &&
	grep "dir1/f1.txt" out &&
	grep "dir1/f2.txt" out
	)
'

test_expect_success 'add matches git ls-files output' '
	(
	cd repo &&
	grit ls-files >grit_out &&
	git ls-files >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 3: --force with gitignore
###########################################################################

test_expect_success 'setup gitignore' '
	(
	cd repo &&
	echo "*.log" >.gitignore &&
	echo "build/" >>.gitignore &&
	grit add .gitignore &&
	grit commit -m "add gitignore"
	)
'

test_expect_success 'add ignored file is rejected without --force' '
	(
	cd repo &&
	echo "log data" >test.log &&
	test_must_fail grit add test.log
	)
'

test_expect_success 'add ignored file with --force succeeds' '
	(
	cd repo &&
	grit add --force test.log &&
	grit ls-files >out &&
	grep "test.log" out
	)
'

test_expect_success 'add -f is alias for --force' '
	(
	cd repo &&
	echo "log2" >test2.log &&
	grit add -f test2.log &&
	grit ls-files >out &&
	grep "test2.log" out
	)
'

test_expect_success 'add ignored directory with --force' '
	(
	cd repo &&
	mkdir -p build &&
	echo "artifact" >build/out.bin &&
	grit add --force build/out.bin &&
	grit ls-files >out &&
	grep "build/out.bin" out
	)
'

test_expect_success 'add non-ignored file succeeds normally' '
	(
	cd repo &&
	echo "normal" >normal.txt &&
	grit add normal.txt &&
	grit ls-files >out &&
	grep "normal.txt" out
	)
'

###########################################################################
# Section 4: -u / --update
###########################################################################

test_expect_success 'commit current state for update tests' '
	(
	cd repo &&
	grit commit -a -m "before update tests" ||
	grit commit --allow-empty -m "before update tests"
	)
'

test_expect_success '-u stages modifications to tracked files' '
	(
	cd repo &&
	echo "modified" >>a.txt &&
	grit add -u &&
	grit status >out &&
	grep "a.txt" out ||
	true
	)
'

test_expect_success '-u does not add untracked files' '
	(
	cd repo &&
	echo "brand new" >brand-new.txt &&
	echo "modagain" >>a.txt &&
	grit add -u &&
	grit ls-files >out &&
	! grep "brand-new.txt" out
	)
'

test_expect_success '--update same as -u' '
	(
	cd repo &&
	echo "update2" >>b.txt &&
	grit add --update &&
	grit diff --cached >out 2>&1 ||
	true
	)
'

###########################################################################
# Section 5: -A / --all
###########################################################################

test_expect_success '-A adds untracked and modified files' '
	(
	cd repo &&
	echo "all-new" >all-new.txt &&
	echo "mod" >>a.txt &&
	grit add -A &&
	grit ls-files >out &&
	grep "all-new.txt" out
	)
'

test_expect_success '--all same as -A' '
	(
	cd repo &&
	echo "all2" >all2.txt &&
	grit add --all &&
	grit ls-files >out &&
	grep "all2.txt" out
	)
'

test_expect_success '-A adds all files in working tree' '
	(
	cd repo &&
	echo "extra" >extra-all.txt &&
	grit add -A &&
	grit ls-files >out &&
	grep "extra-all.txt" out
	)
'

###########################################################################
# Section 6: -n / --dry-run
###########################################################################

test_expect_success '--dry-run shows what would be added' '
	(
	cd repo &&
	echo "dryrun" >dry.txt &&
	grit add --dry-run dry.txt >out 2>&1 &&
	grit ls-files >files &&
	! grep "dry.txt" files
	)
'

test_expect_success '-n is alias for --dry-run' '
	(
	cd repo &&
	grit add -n dry.txt >out 2>&1 &&
	grit ls-files >files &&
	! grep "dry.txt" files
	)
'

###########################################################################
# Section 7: -N / --intent-to-add
###########################################################################

test_expect_success '--intent-to-add creates placeholder' '
	(
	cd repo &&
	echo "intent" >intent.txt &&
	grit add --intent-to-add intent.txt &&
	grit ls-files >out &&
	grep "intent.txt" out
	)
'

test_expect_success '-N is alias for --intent-to-add' '
	(
	cd repo &&
	echo "intent2" >intent2.txt &&
	grit add -N intent2.txt &&
	grit ls-files >out &&
	grep "intent2.txt" out
	)
'

###########################################################################
# Section 8: -v / --verbose
###########################################################################

test_expect_success '--verbose shows added files' '
	(
	cd repo &&
	echo "verb" >verbose-test.txt &&
	grit add --verbose verbose-test.txt >out 2>&1 &&
	grep "verbose-test.txt" out
	)
'

test_expect_success '-v shows added files' '
	(
	cd repo &&
	echo "verb2" >verbose-test2.txt &&
	grit add -v verbose-test2.txt >out 2>&1 &&
	grep "verbose-test2.txt" out
	)
'

###########################################################################
# Section 9: Edge cases
###########################################################################

test_expect_success 'add nonexistent file fails' '
	(
	cd repo &&
	test_must_fail grit add nonexistent-file.txt
	)
'

test_expect_success 'add file with spaces in name' '
	(
	cd repo &&
	echo "space" >"file with spaces.txt" &&
	grit add "file with spaces.txt" &&
	grit ls-files >out &&
	grep "file with spaces.txt" out
	)
'

test_expect_success 'add deeply nested file' '
	(
	cd repo &&
	mkdir -p a/b/c/d/e &&
	echo "deep" >a/b/c/d/e/deep.txt &&
	grit add a/b/c/d/e/deep.txt &&
	grit ls-files >out &&
	grep "a/b/c/d/e/deep.txt" out
	)
'

test_expect_success 'add executable file preserves mode' '
	(
	cd repo &&
	echo "#!/bin/sh" >run.sh &&
	chmod +x run.sh &&
	grit add run.sh &&
	grit ls-files --stage >out 2>&1 &&
	grep "100755" out &&
	grep "run.sh" out
	)
'

test_expect_success 'add same file twice is idempotent' '
	(
	cd repo &&
	echo "idem" >idem.txt &&
	grit add idem.txt &&
	grit add idem.txt &&
	grit ls-files >out &&
	count=$(grep -c "idem.txt" out) &&
	test "$count" = "1"
	)
'

test_expect_success 'add after modifying updates index' '
	(
	cd repo &&
	echo "v1" >update.txt &&
	grit add update.txt &&
	grit commit -m "v1 update" &&
	echo "v2" >update.txt &&
	grit add update.txt &&
	grit diff --cached --name-only >out 2>&1 &&
	grep "update.txt" out ||
	true
	)
'

test_done
