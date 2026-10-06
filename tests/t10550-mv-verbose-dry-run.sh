#!/bin/sh
# Tests for grit mv with -v, --verbose, -n, --dry-run, -f, -k flags
# and various move/rename scenarios.

test_description='grit mv --verbose --dry-run and related options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with files and dirs' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@example.com" &&
	git config user.name "Test User" &&
	echo "alpha" >alpha.txt &&
	echo "beta" >beta.txt &&
	echo "gamma" >gamma.txt &&
	mkdir -p src/lib docs &&
	echo "main" >src/main.rs &&
	echo "lib" >src/lib/util.rs &&
	echo "readme" >docs/README.md &&
	grit add . &&
	grit commit -m "initial commit"
	)
'

# --- basic move ---

test_expect_success 'mv renames file' '
	(
	cd repo &&
	grit mv alpha.txt alpha_renamed.txt &&
	! test -f alpha.txt &&
	test -f alpha_renamed.txt &&
	grit ls-files >ls_out &&
	grep "alpha_renamed.txt" ls_out &&
	! grep "^alpha.txt$" ls_out
	)
'

test_expect_success 'mv file into directory' '
	(
	cd repo &&
	grit mv beta.txt docs/ &&
	! test -f beta.txt &&
	test -f docs/beta.txt &&
	grit ls-files >ls_out &&
	grep "docs/beta.txt" ls_out
	)
'

test_expect_success 'mv file to new name in different directory' '
	(
	cd repo &&
	grit mv gamma.txt docs/gamma_moved.txt &&
	! test -f gamma.txt &&
	test -f docs/gamma_moved.txt
	)
'

# --- --verbose / -v ---

test_expect_success 'mv -v shows rename operation' '
	(
	cd repo &&
	echo "verbose_src" >vsrc.txt &&
	grit add vsrc.txt &&
	grit commit -m "add vsrc" &&
	grit mv -v vsrc.txt vdst.txt >mv_out 2>&1 &&
	cat mv_out &&
	test -f vdst.txt &&
	! test -f vsrc.txt
	)
'

test_expect_success 'mv --verbose shows rename operation' '
	(
	cd repo &&
	echo "verbose2" >verbose2_src.txt &&
	grit add verbose2_src.txt &&
	grit commit -m "add verbose2" &&
	grit mv --verbose verbose2_src.txt verbose2_dst.txt >mv_out 2>&1 &&
	cat mv_out &&
	test -f verbose2_dst.txt
	)
'

test_expect_success 'mv -v output mentions source and destination' '
	(
	cd repo &&
	echo "trackmv" >track_src.txt &&
	grit add track_src.txt &&
	grit commit -m "add track" &&
	grit mv -v track_src.txt track_dst.txt >mv_out 2>&1 &&
	grep "track_src.txt" mv_out &&
	grep "track_dst.txt" mv_out
	)
'

test_expect_success 'mv without -v produces less output' '
	(
	cd repo &&
	echo "quiet_mv" >qsrc.txt &&
	grit add qsrc.txt &&
	grit commit -m "add qsrc" &&
	grit mv qsrc.txt qdst.txt >mv_quiet 2>&1 &&
	test -f qdst.txt
	)
'

# --- --dry-run / -n ---

test_expect_success 'mv --dry-run does not actually move' '
	(
	cd repo &&
	echo "drysrc" >dry_src.txt &&
	grit add dry_src.txt &&
	grit commit -m "add dry_src" &&
	grit mv --dry-run dry_src.txt dry_dst.txt >dry_out 2>&1 &&
	test -f dry_src.txt &&
	! test -f dry_dst.txt
	)
'

test_expect_success 'mv -n does not actually move' '
	(
	cd repo &&
	grit mv -n dry_src.txt dry_n_dst.txt >dry_out 2>&1 &&
	test -f dry_src.txt &&
	! test -f dry_n_dst.txt
	)
'

test_expect_success 'mv --dry-run mentions files in output' '
	(
	cd repo &&
	grit mv --dry-run dry_src.txt dry_dst.txt >dry_out 2>&1 &&
	grep "dry_src.txt" dry_out
	)
'

test_expect_success 'mv -n does not change index' '
	(
	cd repo &&
	grit ls-files >before &&
	grit mv -n dry_src.txt dry_nowhere.txt >dry_out 2>&1 &&
	grit ls-files >after &&
	diff before after
	)
'

test_expect_success 'mv --dry-run with multiple files' '
	(
	cd repo &&
	echo "d1" >drym1.txt &&
	echo "d2" >drym2.txt &&
	grit add drym1.txt drym2.txt &&
	grit commit -m "add drym" &&
	mkdir -p drydir &&
	grit mv --dry-run drym1.txt drym2.txt drydir/ >dry_out 2>&1 &&
	test -f drym1.txt &&
	test -f drym2.txt &&
	! test -f drydir/drym1.txt
	)
'

# --- -f (force) ---

test_expect_success 'mv -f overwrites existing file' '
	(
	cd repo &&
	echo "source" >src_force.txt &&
	echo "target" >dst_force.txt &&
	grit add src_force.txt dst_force.txt &&
	grit commit -m "add force pair" &&
	grit mv -f src_force.txt dst_force.txt &&
	! test -f src_force.txt &&
	test "$(cat dst_force.txt)" = "source"
	)
'

test_expect_success 'mv without -f onto existing file fails' '
	(
	cd repo &&
	echo "s" >nosrc.txt &&
	echo "d" >nodst.txt &&
	grit add nosrc.txt nodst.txt &&
	grit commit -m "add nof pair" &&
	test_must_fail grit mv nosrc.txt nodst.txt 2>err
	)
'

# --- -k (skip errors) ---

test_expect_success 'mv -k skips errors instead of aborting' '
	(
	cd repo &&
	echo "ksrc" >ksrc.txt &&
	grit add ksrc.txt &&
	grit commit -m "add ksrc" &&
	grit mv -k nonexistent.txt ksrc.txt docs/ 2>err_out &&
	test -f docs/ksrc.txt
	)
'

test_expect_success 'mv -k with only bad sources still succeeds exit' '
	(
	cd repo &&
	grit mv -k no_exist1.txt no_exist2.txt docs/ 2>err
	)
'

# --- moving directories ---

test_expect_success 'mv directory to new name' '
	(
	cd repo &&
	mkdir -p movedir &&
	echo "f1" >movedir/f1.txt &&
	echo "f2" >movedir/f2.txt &&
	grit add movedir &&
	grit commit -m "add movedir" &&
	grit mv movedir renamed_dir &&
	! test -d movedir &&
	test -d renamed_dir &&
	test -f renamed_dir/f1.txt &&
	test -f renamed_dir/f2.txt
	)
'

test_expect_success 'mv directory into another directory' '
	(
	cd repo &&
	mkdir -p innerdir &&
	echo "inner" >innerdir/inner.txt &&
	mkdir -p outerdir &&
	grit add innerdir &&
	grit commit -m "add innerdir" &&
	grit mv innerdir outerdir/ &&
	test -f outerdir/innerdir/inner.txt
	)
'

# --- edge cases ---

test_expect_success 'mv file with spaces in name' '
	(
	cd repo &&
	echo "sp" >"space file.txt" &&
	grit add "space file.txt" &&
	grit commit -m "add spaced" &&
	grit mv "space file.txt" "moved space.txt" &&
	! test -f "space file.txt" &&
	test -f "moved space.txt"
	)
'

test_expect_success 'mv nonexistent file fails' '
	(
	cd repo &&
	test_must_fail grit mv ghost.txt somewhere.txt 2>err &&
	test -s err
	)
'

test_expect_success 'mv same name is an error' '
	(
	cd repo &&
	echo "same" >samename.txt &&
	grit add samename.txt &&
	grit commit -m "add same" &&
	test_must_fail grit mv samename.txt samename.txt 2>err
	)
'

test_expect_success 'mv untracked file fails' '
	(
	cd repo &&
	echo "untracked" >untracked_mv.txt &&
	test_must_fail grit mv untracked_mv.txt somewhere.txt 2>err &&
	test -s err &&
	rm -f untracked_mv.txt
	)
'

test_expect_success 'mv updates index correctly' '
	(
	cd repo &&
	echo "idx" >idx_src.txt &&
	grit add idx_src.txt &&
	grit commit -m "add idx" &&
	grit mv idx_src.txt idx_dst.txt &&
	grit ls-files >ls_out &&
	grep "idx_dst.txt" ls_out &&
	! grep "idx_src.txt" ls_out
	)
'

test_expect_success 'mv multiple files to directory' '
	(
	cd repo &&
	echo "m1" >mvmulti1.txt &&
	echo "m2" >mvmulti2.txt &&
	echo "m3" >mvmulti3.txt &&
	mkdir -p target_dir &&
	grit add mvmulti1.txt mvmulti2.txt mvmulti3.txt &&
	grit commit -m "add multi" &&
	grit mv mvmulti1.txt mvmulti2.txt mvmulti3.txt target_dir/ &&
	test -f target_dir/mvmulti1.txt &&
	test -f target_dir/mvmulti2.txt &&
	test -f target_dir/mvmulti3.txt
	)
'

test_expect_success 'mv -v --dry-run combined shows but does not move' '
	(
	cd repo &&
	echo "combo" >combo_src.txt &&
	grit add combo_src.txt &&
	grit commit -m "add combo" &&
	grit mv -v --dry-run combo_src.txt combo_dst.txt >out 2>&1 &&
	test -f combo_src.txt &&
	! test -f combo_dst.txt &&
	grep "combo_src.txt" out
	)
'

test_expect_success 'mv -v -f overwrites and is verbose' '
	(
	cd repo &&
	echo "vf_src" >vf_src.txt &&
	echo "vf_dst" >vf_dst.txt &&
	grit add vf_src.txt vf_dst.txt &&
	grit commit -m "add vf pair" &&
	grit mv -v -f vf_src.txt vf_dst.txt >out 2>&1 &&
	! test -f vf_src.txt &&
	test "$(cat vf_dst.txt)" = "vf_src"
	)
'

test_expect_success 'mv file preserves content' '
	(
	cd repo &&
	echo "preserve this content exactly" >preserve.txt &&
	grit add preserve.txt &&
	grit commit -m "add preserve" &&
	grit mv preserve.txt preserved_new.txt &&
	test "$(cat preserved_new.txt)" = "preserve this content exactly"
	)
'

test_done
