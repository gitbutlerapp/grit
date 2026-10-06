#!/bin/sh
# Tests for grit mv: rename files, move across directories,
# -f (force), -n (dry-run), -k (skip errors), -v (verbose),
# directory moves, and cross-checks with real git.

test_description='grit mv cross-directory, rename, force, dry-run, verbose'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with files in multiple directories' '
	(
	grit init repo &&
	cd repo &&
	echo "alpha" >alpha.txt &&
	echo "beta" >beta.txt &&
	mkdir -p src/lib &&
	echo "main" >src/main.rs &&
	echo "lib" >src/lib/mod.rs &&
	mkdir dst &&
	echo "existing" >dst/existing.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "initial commit"
	)
'

###########################################################################
# Section 2: Basic rename
###########################################################################

test_expect_success 'mv renames file in same directory' '
	(
	cd repo &&
	grit mv alpha.txt renamed.txt &&
	test -f renamed.txt &&
	! test -f alpha.txt
	)
'

test_expect_success 'mv updates the index after rename' '
	(
	cd repo &&
	grit ls-files >actual &&
	grep "renamed.txt" actual &&
	! grep "alpha.txt" actual
	)
'

test_expect_success 'mv shows rename in status' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	grep "renamed.txt" actual
	)
'

test_expect_success 'commit rename and setup for next tests' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "renamed alpha to renamed"
	)
'

###########################################################################
# Section 3: Move to different directory
###########################################################################

test_expect_success 'mv file into existing directory' '
	(
	cd repo &&
	grit mv renamed.txt dst/ &&
	test -f dst/renamed.txt &&
	! test -f renamed.txt
	)
'

test_expect_success 'mv to dir updates index' '
	(
	cd repo &&
	grit ls-files >actual &&
	grep "dst/renamed.txt" actual &&
	! grep "^renamed.txt$" actual
	)
'

test_expect_success 'commit and continue' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "moved to dst"
	)
'

test_expect_success 'mv file from subdirectory to root' '
	(
	cd repo &&
	grit mv dst/renamed.txt . &&
	test -f renamed.txt &&
	! test -f dst/renamed.txt
	)
'

test_expect_success 'commit move to root' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "moved back to root"
	)
'

###########################################################################
# Section 4: Move across directories (cross-directory)
###########################################################################

test_expect_success 'mv file from one subdir to another' '
	(
	cd repo &&
	grit mv src/main.rs dst/ &&
	test -f dst/main.rs &&
	! test -f src/main.rs
	)
'

test_expect_success 'cross-directory move updates index' '
	(
	cd repo &&
	grit ls-files >actual &&
	grep "dst/main.rs" actual &&
	! grep "src/main.rs" actual
	)
'

test_expect_success 'commit cross-dir move' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "moved src/main.rs to dst/"
	)
'

test_expect_success 'mv deeply nested file to root' '
	(
	cd repo &&
	grit mv src/lib/mod.rs . &&
	test -f mod.rs &&
	! test -f src/lib/mod.rs
	)
'

test_expect_success 'commit deep move' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "moved mod.rs to root"
	)
'

###########################################################################
# Section 5: Directory move
###########################################################################

test_expect_success 'setup: create dir with multiple files' '
	(
	cd repo &&
	mkdir -p moveme &&
	echo "x" >moveme/x.txt &&
	echo "y" >moveme/y.txt &&
	grit add moveme &&
	test_tick &&
	grit commit -m "add moveme dir"
	)
'

test_expect_success 'mv directory into another directory' '
	(
	cd repo &&
	grit mv moveme dst/ &&
	test -f dst/moveme/x.txt &&
	test -f dst/moveme/y.txt &&
	! test -d moveme
	)
'

test_expect_success 'directory move updates index' '
	(
	cd repo &&
	grit ls-files >actual &&
	grep "dst/moveme/x.txt" actual &&
	grep "dst/moveme/y.txt" actual &&
	! grep "^moveme/" actual
	)
'

test_expect_success 'commit dir move' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "moved moveme into dst"
	)
'

###########################################################################
# Section 6: Force move (-f)
###########################################################################

test_expect_success 'mv fails when destination exists' '
	(
	cd repo &&
	echo "new" >conflict.txt &&
	grit add conflict.txt &&
	test_tick &&
	grit commit -m "add conflict" &&
	echo "target" >target.txt &&
	grit add target.txt &&
	test_tick &&
	grit commit -m "add target" &&
	test_must_fail grit mv conflict.txt target.txt
	)
'

test_expect_success 'mv -f overwrites existing destination' '
	(
	cd repo &&
	grit mv -f conflict.txt target.txt &&
	test -f target.txt &&
	! test -f conflict.txt &&
	grep "new" target.txt
	)
'

test_expect_success 'commit force move' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "force moved"
	)
'

test_expect_success 'mv --force is same as -f' '
	(
	cd repo &&
	echo "src" >src_file.txt &&
	echo "dst" >dst_file.txt &&
	grit add src_file.txt dst_file.txt &&
	test_tick &&
	grit commit -m "for force test" &&
	grit mv --force src_file.txt dst_file.txt &&
	! test -f src_file.txt &&
	grep "src" dst_file.txt
	)
'

test_expect_success 'commit --force move' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "force moved again"
	)
'

###########################################################################
# Section 7: Dry-run (-n / --dry-run)
###########################################################################

test_expect_success 'mv --dry-run does not actually move' '
	(
	cd repo &&
	grit mv --dry-run beta.txt dst/ &&
	test -f beta.txt &&
	! test -f dst/beta.txt
	)
'

test_expect_success 'mv --dry-run does not change index' '
	(
	cd repo &&
	grit ls-files >actual &&
	grep "^beta.txt$" actual &&
	! grep "dst/beta.txt" actual
	)
'

test_expect_success 'mv -n is same as --dry-run' '
	(
	cd repo &&
	grit mv -n beta.txt dst/ &&
	test -f beta.txt &&
	grit ls-files >actual &&
	grep "^beta.txt$" actual
	)
'

###########################################################################
# Section 8: Verbose (-v)
###########################################################################

test_expect_success 'mv -v shows what is being moved' '
	(
	cd repo &&
	grit mv -v beta.txt dst/ >out 2>&1 &&
	test -s out
	)
'

test_expect_success 'commit verbose move' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "moved beta to dst"
	)
'

###########################################################################
# Section 9: Skip errors (-k)
###########################################################################

test_expect_success 'mv -k skips errors instead of aborting' '
	(
	cd repo &&
	echo "ok" >ok.txt &&
	grit add ok.txt &&
	test_tick &&
	grit commit -m "add ok" &&
	grit mv -k nonexistent.txt ok.txt dst/ 2>err &&
	test -f dst/ok.txt
	)
'

test_expect_success 'commit -k move' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "moved with -k"
	)
'

###########################################################################
# Section 10: Error cases
###########################################################################

test_expect_success 'mv nonexistent file fails' '
	(
	cd repo &&
	test_must_fail grit mv no-such-file.txt dst/
	)
'

test_expect_success 'mv to nonexistent directory fails' '
	(
	cd repo &&
	echo "tmp" >tmp.txt &&
	grit add tmp.txt &&
	test_must_fail grit mv tmp.txt no-such-dir/
	)
'

test_expect_success 'mv with no arguments fails' '
	(
	cd repo &&
	test_must_fail grit mv
	)
'

test_expect_success 'mv file onto itself fails' '
	(
	cd repo &&
	test_must_fail grit mv renamed.txt renamed.txt
	)
'

test_expect_success 'mv with rename preserves file content' '
	(
	cd repo &&
	grit checkout HEAD -- tmp.txt 2>/dev/null || true &&
	echo "content check" >content.txt &&
	grit add content.txt &&
	test_tick &&
	grit commit -m "add content" &&
	grit mv content.txt content-moved.txt &&
	grep "content check" content-moved.txt
	)
'

###########################################################################
# Section 11: Cross-check with real git
###########################################################################

test_expect_success 'setup cross-check repos' '
	(
	$REAL_GIT init git-repo &&
	cd git-repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	mkdir sub &&
	echo "c" >sub/c.txt &&
	$REAL_GIT add . &&
	test_tick &&
	$REAL_GIT commit -m "init" &&
	cd .. &&
	grit init grit-repo &&
	cd grit-repo &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	mkdir sub &&
	echo "c" >sub/c.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "init"
	)
'

test_expect_success 'mv rename: grit matches real git ls-files' '
	$REAL_GIT -C git-repo mv a.txt renamed-a.txt &&
	grit -C grit-repo mv a.txt renamed-a.txt &&
	$REAL_GIT -C git-repo ls-files >expect &&
	grit -C grit-repo ls-files >actual &&
	test_cmp expect actual
'

test_expect_success 'mv cross-dir: grit matches real git ls-files' '
	$REAL_GIT -C git-repo mv sub/c.txt . &&
	grit -C grit-repo mv sub/c.txt . &&
	$REAL_GIT -C git-repo ls-files >expect &&
	grit -C grit-repo ls-files >actual &&
	test_cmp expect actual
'

test_expect_success 'mv to subdir: grit matches real git ls-files' '
	$REAL_GIT -C git-repo mv b.txt sub/ &&
	grit -C grit-repo mv b.txt sub/ &&
	$REAL_GIT -C git-repo ls-files >expect &&
	grit -C grit-repo ls-files >actual &&
	test_cmp expect actual
'

test_done
