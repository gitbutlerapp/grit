#!/bin/sh
# Test add with pathspec patterns, --pathspec-from-file (skipped if unsupported),
# and various add flags (-u, -A, -n, -N, -f, -v).

test_description='grit add pathspec patterns and flags'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with structure' '
	(
	grit init add-repo &&
	cd add-repo &&
	grit config user.email "test@test.com" &&
	grit config user.name "Test" &&
	mkdir -p src lib docs &&
	echo "main" >src/main.c &&
	echo "util" >src/util.c &&
	echo "helper" >lib/helper.h &&
	echo "readme" >docs/README.md &&
	echo "root" >root.txt &&
	grit add . &&
	grit commit -m "initial structure"
	)
'

###########################################################################
# Section 2: --pathspec-from-file (skip if unsupported)
###########################################################################

test_expect_success 'add --pathspec-from-file stages listed paths' '
	(
	cd add-repo &&
	echo "src/main.c" >pathspec.txt &&
	echo "modified-main" >src/main.c &&
	grit add --pathspec-from-file=pathspec.txt &&
	grit diff --cached --name-only >staged &&
	grep src/main.c staged
	)
'

###########################################################################
# Section 3: Basic pathspec patterns
###########################################################################

test_expect_success 'add single file by path' '
	(
	cd add-repo &&
	echo "modified-main" >src/main.c &&
	grit add src/main.c &&
	grit status >out &&
	grep "src/main.c" out &&
	grit commit -m "mod main"
	)
'

test_expect_success 'add multiple files by path' '
	(
	cd add-repo &&
	echo "mod-util" >src/util.c &&
	echo "mod-helper" >lib/helper.h &&
	grit add src/util.c lib/helper.h &&
	grit status >out &&
	grep "src/util.c" out &&
	grep "lib/helper.h" out &&
	grit commit -m "mod two files"
	)
'

test_expect_success 'add entire directory' '
	(
	cd add-repo &&
	echo "new-main" >src/main.c &&
	echo "new-util" >src/util.c &&
	grit add src/ &&
	grit status >out &&
	grep "src/main.c" out &&
	grep "src/util.c" out &&
	grit commit -m "mod src dir"
	)
'

test_expect_success 'add . adds everything' '
	(
	cd add-repo &&
	echo "mod-root" >root.txt &&
	echo "mod-readme" >docs/README.md &&
	grit add . &&
	grit status >out &&
	grep "root.txt" out &&
	grep "docs/README.md" out &&
	grit commit -m "add dot"
	)
'

test_expect_success 'add new file in subdirectory' '
	(
	cd add-repo &&
	echo "new-file" >src/new.c &&
	grit add src/new.c &&
	grit status >out &&
	grep "src/new.c" out &&
	grit commit -m "add new file"
	)
'

###########################################################################
# Section 4: add -u (update tracked only)
###########################################################################

test_expect_success 'add -u updates modified tracked files' '
	(
	cd add-repo &&
	echo "updated-main" >src/main.c &&
	echo "untracked" >untracked.txt &&
	grit add -u &&
	grit status >out &&
	grep "src/main.c" out &&
	grit commit -m "add -u" &&
	grit status >out2 &&
	grep "untracked.txt" out2
	)
'

test_expect_success 'add -u stages deletions' '
	(
	cd add-repo &&
	rm src/new.c &&
	grit add -u &&
	grit status >out &&
	grep "src/new.c" out &&
	grit commit -m "delete via add -u"
	)
'

test_expect_success 'add -u does not add new untracked files' '
	(
	cd add-repo &&
	echo "brand-new" >brand-new.txt &&
	grit add -u &&
	grit status >out &&
	grep "brand-new.txt" out &&
	rm -f brand-new.txt &&
	rm -f untracked.txt
	)
'

###########################################################################
# Section 5: add -A (all changes)
###########################################################################

test_expect_success 'add -A stages new, modified, and deleted files' '
	(
	grit init add-a-repo &&
	cd add-a-repo &&
	grit config user.email "test@test.com" &&
	grit config user.name "Test" &&
	echo "orig" >keep.txt &&
	echo "del" >remove.txt &&
	grit add . &&
	grit commit -m "setup" &&
	echo "new" >new.txt &&
	echo "modified" >keep.txt &&
	rm remove.txt &&
	grit add -A &&
	grit status >out &&
	grep "new.txt" out &&
	grep "keep.txt" out &&
	grep "remove.txt" out &&
	grit commit -m "add -A done"
	)
'

###########################################################################
# Section 6: add -n (dry-run)
###########################################################################

test_expect_success 'add -n shows what would be added without staging' '
	(
	cd add-repo &&
	echo "dryrun" >dryrun.txt &&
	grit add -n dryrun.txt >out 2>&1 &&
	grit status >status_out &&
	grep "dryrun.txt" status_out &&
	rm dryrun.txt
	)
'

test_expect_success 'add -n does not modify index' '
	(
	cd add-repo &&
	echo "dryrun2" >dryrun2.txt &&
	grit ls-files >before &&
	grit add -n dryrun2.txt 2>&1 &&
	grit ls-files >after &&
	test_cmp before after &&
	rm dryrun2.txt
	)
'

###########################################################################
# Section 7: add -N (intent-to-add)
###########################################################################

test_expect_success 'add -N marks file as intent-to-add' '
	(
	cd add-repo &&
	echo "intent" >intent.txt &&
	grit add -N intent.txt &&
	grit ls-files >out &&
	grep "intent.txt" out
	)
'

test_expect_success 'add -N file shows in status' '
	(
	cd add-repo &&
	grit status >out &&
	grep "intent.txt" out
	)
'

test_expect_success 'add file after -N stages it fully' '
	(
	cd add-repo &&
	grit add intent.txt &&
	grit commit -m "add intent file" &&
	grit ls-files >out &&
	grep "intent.txt" out
	)
'

###########################################################################
# Section 8: add -v (verbose)
###########################################################################

test_expect_success 'add -v shows added files' '
	(
	cd add-repo &&
	echo "verbose" >verbose.txt &&
	grit add -v verbose.txt >out 2>&1 &&
	grep "verbose.txt" out &&
	grit commit -m "add verbose"
	)
'

###########################################################################
# Section 9: add -f (force, bypass ignore)
###########################################################################

test_expect_success 'add -f adds ignored files' '
	(
	cd add-repo &&
	echo "*.log" >.gitignore &&
	grit add .gitignore &&
	grit commit -m "add gitignore" &&
	echo "logdata" >test.log &&
	grit add -f test.log &&
	grit status >out &&
	grep "test.log" out &&
	grit commit -m "force add log"
	)
'

test_expect_success 'add ignored file is rejected without -f' '
	(
	cd add-repo &&
	echo "another-log" >another.log &&
	test_must_fail grit add another.log 2>err &&
	grep -i "ignored" err &&
	rm -f another.log
	)
'

###########################################################################
# Section 10: Edge cases
###########################################################################

test_expect_success 'add nonexistent file fails' '
	(
	cd add-repo &&
	test_must_fail grit add nonexistent-file.txt 2>err
	)
'

test_expect_success 'add empty directory is a no-op' '
	(
	cd add-repo &&
	mkdir -p empty-dir &&
	grit add empty-dir &&
	grit ls-files >out &&
	! grep "empty-dir" out &&
	rmdir empty-dir
	)
'

test_expect_success 'add file with spaces in name' '
	(
	cd add-repo &&
	echo "spaces" >"file with spaces.txt" &&
	grit add "file with spaces.txt" &&
	grit ls-files >out &&
	grep "file with spaces.txt" out &&
	grit commit -m "file with spaces"
	)
'

test_expect_success 'add deeply nested file' '
	(
	cd add-repo &&
	mkdir -p a/b/c/d &&
	echo "deep" >a/b/c/d/deep.txt &&
	grit add a/b/c/d/deep.txt &&
	grit ls-files >out &&
	grep "a/b/c/d/deep.txt" out &&
	grit commit -m "deep file"
	)
'

test_expect_success 'add same file twice is idempotent' '
	(
	cd add-repo &&
	echo "twice" >twice.txt &&
	grit add twice.txt &&
	grit add twice.txt &&
	grit commit -m "twice"
	)
'

test_done
