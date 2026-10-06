#!/bin/sh
# Test diff-tree with pathspec filtering: directory, glob, extension,
# exclusion, and combinations.

test_description='grit diff-tree with pathspec filtering'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup repository with nested structure' '
	(
	grit init repo &&
	cd repo &&
	$REAL_GIT config user.email "test@test.com" &&
	$REAL_GIT config user.name "Tester" &&
	mkdir -p src/lib src/bin docs tests &&
	echo "main" >src/bin/main.rs &&
	echo "lib" >src/lib/lib.rs &&
	echo "util" >src/lib/util.rs &&
	echo "helper" >src/lib/helper.c &&
	echo "readme" >docs/README.md &&
	echo "guide" >docs/guide.txt &&
	echo "test1" >tests/test1.rs &&
	echo "test2" >tests/test2.rs &&
	echo "root" >root.txt &&
	grit add . &&
	grit commit -m "initial structure" &&
	grit rev-parse HEAD >../SHA_INIT
	)
'

test_expect_success 'setup second commit modifying multiple paths' '
	(
	cd repo &&
	echo "main v2" >src/bin/main.rs &&
	echo "lib v2" >src/lib/lib.rs &&
	echo "readme v2" >docs/README.md &&
	echo "test1 v2" >tests/test1.rs &&
	echo "root v2" >root.txt &&
	grit add . &&
	grit commit -m "modify several files" &&
	grit rev-parse HEAD >../SHA_MOD
	)
'

###########################################################################
# Section 1: Directory pathspec
###########################################################################

test_expect_success 'diff-tree with src/ pathspec shows only src changes' '
	(
	cd repo &&
	grit diff-tree -r $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- src/ >out &&
	grep "src/bin/main.rs" out &&
	grep "src/lib/lib.rs" out &&
	! grep "docs/" out &&
	! grep "tests/" out &&
	! grep "root.txt" out
	)
'

test_expect_success 'diff-tree with docs/ pathspec shows only docs changes' '
	(
	cd repo &&
	grit diff-tree -r $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- docs/ >out &&
	grep "docs/README.md" out &&
	! grep "src/" out
	)
'

test_expect_success 'diff-tree with tests/ pathspec shows only test changes' '
	(
	cd repo &&
	grit diff-tree -r $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- tests/ >out &&
	grep "tests/test1.rs" out &&
	! grep "src/" out &&
	! grep "docs/" out
	)
'

test_expect_success 'diff-tree with nested dir src/lib/ pathspec' '
	(
	cd repo &&
	grit diff-tree -r $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- src/lib/ >out &&
	grep "src/lib/lib.rs" out &&
	! grep "src/bin/" out
	)
'

test_expect_success 'diff-tree with src/bin/ pathspec' '
	(
	cd repo &&
	grit diff-tree -r $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- src/bin/ >out &&
	grep "src/bin/main.rs" out &&
	! grep "src/lib/" out
	)
'

###########################################################################
# Section 2: Specific file pathspec
###########################################################################

test_expect_success 'diff-tree with exact file path' '
	(
	cd repo &&
	grit diff-tree -r $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- root.txt >out &&
	grep "root.txt" out &&
	test_line_count = 1 out
	)
'

test_expect_success 'diff-tree with exact nested file path' '
	(
	cd repo &&
	grit diff-tree -r $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- src/bin/main.rs >out &&
	grep "src/bin/main.rs" out &&
	test_line_count = 1 out
	)
'

test_expect_success 'diff-tree with non-matching pathspec produces empty output' '
	(
	cd repo &&
	grit diff-tree -r $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- nonexistent/ >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 3: Multiple pathspecs
###########################################################################

test_expect_success 'diff-tree with two directory pathspecs' '
	(
	cd repo &&
	grit diff-tree -r $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- src/ docs/ >out &&
	grep "src/bin/main.rs" out &&
	grep "docs/README.md" out &&
	! grep "tests/" out &&
	! grep "root.txt" out
	)
'

test_expect_success 'diff-tree with mixed file and dir pathspecs' '
	(
	cd repo &&
	grit diff-tree -r $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- root.txt docs/ >out &&
	grep "root.txt" out &&
	grep "docs/README.md" out &&
	! grep "src/" out
	)
'

###########################################################################
# Section 4: Pathspec with --name-only
###########################################################################

test_expect_success 'diff-tree --name-only with pathspec' '
	(
	cd repo &&
	grit diff-tree -r --name-only $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- src/ >out &&
	grep "^src/bin/main.rs$" out &&
	grep "^src/lib/lib.rs$" out &&
	! grep "docs" out
	)
'

test_expect_success 'diff-tree --name-only with exact file pathspec' '
	(
	cd repo &&
	grit diff-tree -r --name-only $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- root.txt >out &&
	grep "^root.txt$" out &&
	test_line_count = 1 out
	)
'

###########################################################################
# Section 5: Pathspec with --name-status
###########################################################################

test_expect_success 'diff-tree --name-status with pathspec shows status' '
	(
	cd repo &&
	grit diff-tree -r --name-status $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- src/lib/ >out &&
	grep "M" out &&
	grep "lib.rs" out
	)
'

test_expect_success 'diff-tree --name-status with pathspec for addition' '
	(
	cd repo &&
	echo "new" >src/lib/new.rs &&
	grit add src/lib/new.rs &&
	grit commit -m "add new.rs" &&
	grit diff-tree -r --name-status HEAD~1 HEAD -- src/lib/ >out &&
	grep "A" out &&
	grep "new.rs" out
	)
'

test_expect_success 'diff-tree --name-status with pathspec for deletion' '
	(
	cd repo &&
	grit rm src/lib/helper.c &&
	grit commit -m "remove helper" &&
	grit diff-tree -r --name-status HEAD~1 HEAD -- src/lib/ >out &&
	grep "D" out &&
	grep "helper.c" out
	)
'

###########################################################################
# Section 6: Pathspec with --stat
###########################################################################

test_expect_success 'diff-tree --stat with pathspec' '
	(
	cd repo &&
	grit diff-tree -r --stat $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- src/ >out &&
	grep "src" out &&
	! grep "root.txt" out
	)
'

test_expect_success 'diff-tree --stat with pathspec shows summary' '
	(
	cd repo &&
	grit diff-tree -r --stat $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- src/ >out &&
	grep "file" out &&
	grep "changed" out
	)
'

###########################################################################
# Section 7: Pathspec with -p (patch)
###########################################################################

test_expect_success 'diff-tree -p with pathspec shows only matching diffs' '
	(
	cd repo &&
	grit diff-tree -r -p $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- src/bin/ >out &&
	grep "diff --git" out &&
	grep "main.rs" out &&
	! grep "lib.rs" out
	)
'

test_expect_success 'diff-tree -p with docs/ pathspec' '
	(
	cd repo &&
	grit diff-tree -r -p $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- docs/ >out &&
	grep "diff --git" out &&
	grep "README.md" out &&
	! grep "main.rs" out
	)
'

###########################################################################
# Section 8: Pathspec with single-commit diff-tree
###########################################################################

test_expect_success 'diff-tree single commit with pathspec filters output' '
	(
	cd repo &&
	grit diff-tree $(cat ../SHA_MOD) -- src/ >out &&
	grep "src" out &&
	! grep "root.txt" out
	)
'

test_expect_success 'diff-tree single commit with non-matching pathspec is empty' '
	(
	cd repo &&
	grit diff-tree $(cat ../SHA_MOD) -- nonexistent/ >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 9: Pathspec with additions/deletions across dirs
###########################################################################

test_expect_success 'setup: add files in new dirs and remove in existing' '
	(
	cd repo &&
	mkdir -p extra &&
	echo "extra1" >extra/e1.txt &&
	echo "extra2" >extra/e2.txt &&
	grit rm tests/test2.rs &&
	grit add extra/ &&
	grit commit -m "add extra, remove test2"
	)
'

test_expect_success 'diff-tree pathspec extra/ shows only additions' '
	(
	cd repo &&
	grit diff-tree -r --name-status HEAD~1 HEAD -- extra/ >out &&
	grep "A" out &&
	grep "e1.txt" out &&
	grep "e2.txt" out &&
	! grep "test2" out
	)
'

test_expect_success 'diff-tree pathspec tests/ shows only deletion' '
	(
	cd repo &&
	grit diff-tree -r --name-status HEAD~1 HEAD -- tests/ >out &&
	grep "D" out &&
	grep "test2.rs" out &&
	! grep "extra" out
	)
'

###########################################################################
# Section 10: Edge cases
###########################################################################

test_expect_success 'diff-tree with pathspec matching unchanged file is empty' '
	(
	cd repo &&
	grit diff-tree -r $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- src/lib/util.rs >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff-tree with root-level file pathspec' '
	(
	cd repo &&
	grit diff-tree -r --name-only $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- root.txt >out &&
	test_line_count = 1 out &&
	grep "root.txt" out
	)
'

test_expect_success 'diff-tree -r with pathspec across many commits' '
	(
	cd repo &&
	grit diff-tree -r $(cat ../SHA_INIT) HEAD -- src/ >out &&
	test -s out
	)
'

test_expect_success 'diff-tree pathspec does not match partial directory names' '
	(
	cd repo &&
	grit diff-tree -r $(cat ../SHA_INIT) $(cat ../SHA_MOD) -- doc >out &&
	test_must_be_empty out
	)
'

test_done
