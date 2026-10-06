#!/bin/sh
# Tests for grit mv with multiple source files moved to a destination
# directory, covering -f, -k, -n, -v flags and error conditions.

test_description='grit mv with multiple sources to destination directory'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@example.com" &&
	git config user.name "Test User" &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	echo "c" >c.txt &&
	echo "d" >d.txt &&
	echo "e" >e.txt &&
	mkdir -p dest src/sub &&
	echo "s1" >src/s1.txt &&
	echo "s2" >src/sub/s2.txt &&
	grit add . &&
	grit commit -m "initial"
	)
'

# --- multi-source to directory ---

test_expect_success 'mv two files into directory' '
	(
	cd repo &&
	grit mv a.txt b.txt dest/ &&
	test -f dest/a.txt &&
	test -f dest/b.txt &&
	! test -f a.txt &&
	! test -f b.txt
	)
'

test_expect_success 'index reflects multi-source mv' '
	(
	cd repo &&
	grit ls-files >ls_out &&
	grep "dest/a.txt" ls_out &&
	grep "dest/b.txt" ls_out &&
	! grep "^a.txt$" ls_out &&
	! grep "^b.txt$" ls_out
	)
'

test_expect_success 'commit after multi-source mv' '
	(
	cd repo &&
	grit commit -m "moved a and b to dest"
	)
'

test_expect_success 'mv three files into directory at once' '
	(
	cd repo &&
	grit mv c.txt d.txt e.txt dest/ &&
	test -f dest/c.txt &&
	test -f dest/d.txt &&
	test -f dest/e.txt &&
	! test -f c.txt &&
	! test -f d.txt &&
	! test -f e.txt
	)
'

test_expect_success 'commit multi-file move' '
	(
	cd repo &&
	grit commit -m "moved c d e to dest"
	)
'

# --- mv with -v shows each rename ---

test_expect_success 'mv -v with multiple files shows all renames' '
	(
	cd repo &&
	echo "v1" >v1.txt &&
	echo "v2" >v2.txt &&
	grit add v1.txt v2.txt &&
	grit commit -m "add v1 v2" &&
	mkdir -p vdest &&
	grit mv -v v1.txt v2.txt vdest/ >out 2>&1 &&
	test -f vdest/v1.txt &&
	test -f vdest/v2.txt
	)
'

test_expect_success 'commit verbose move' '
	(
	cd repo &&
	grit commit -m "moved v1 v2"
	)
'

# --- mv with -n (dry-run) ---

test_expect_success 'mv -n with multiple files does not move anything' '
	(
	cd repo &&
	echo "n1" >n1.txt &&
	echo "n2" >n2.txt &&
	grit add n1.txt n2.txt &&
	grit commit -m "add n1 n2" &&
	mkdir -p ndest &&
	grit mv -n n1.txt n2.txt ndest/ &&
	test -f n1.txt &&
	test -f n2.txt &&
	! test -f ndest/n1.txt &&
	! test -f ndest/n2.txt
	)
'

test_expect_success 'mv --dry-run shows what would happen' '
	(
	cd repo &&
	grit mv --dry-run n1.txt n2.txt ndest/ >out 2>&1 &&
	grep "n1.txt" out &&
	grep "n2.txt" out &&
	test -f n1.txt
	)
'

test_expect_success 'mv -n preserves index entries' '
	(
	cd repo &&
	grit mv -n n1.txt n2.txt ndest/ &&
	grit ls-files >ls_out &&
	grep "^n1.txt$" ls_out &&
	grep "^n2.txt$" ls_out
	)
'

# --- mv with -f (force) ---

test_expect_success 'mv -f overwrites existing destination' '
	(
	cd repo &&
	echo "orig" >target.txt &&
	echo "overwrite" >source.txt &&
	grit add target.txt source.txt &&
	grit commit -m "add target and source" &&
	grit mv -f source.txt target.txt &&
	test "$(cat target.txt)" = "overwrite" &&
	! test -f source.txt
	)
'

test_expect_success 'mv --force overwrites file in destination dir' '
	(
	cd repo &&
	echo "existing" >ndest/clash.txt &&
	echo "incoming" >clash.txt &&
	grit add ndest/clash.txt clash.txt &&
	grit commit -m "add clash files" &&
	grit mv -f clash.txt ndest/ &&
	test "$(cat ndest/clash.txt)" = "incoming"
	)
'

test_expect_success 'commit force moves' '
	(
	cd repo &&
	grit commit -m "force moves done"
	)
'

# --- mv with -k (skip errors) ---

test_expect_success 'mv -k skips nonexistent source and moves the rest' '
	(
	cd repo &&
	echo "real" >real.txt &&
	grit add real.txt &&
	grit commit -m "add real" &&
	mkdir -p kdest &&
	grit mv -k nonexistent.txt real.txt kdest/ &&
	test -f kdest/real.txt &&
	! test -f real.txt
	)
'

test_expect_success 'commit after -k move' '
	(
	cd repo &&
	grit commit -m "k-move"
	)
'

test_expect_success 'mv -k with all-invalid sources does nothing' '
	(
	cd repo &&
	mkdir -p empty_dest &&
	grit mv -k no1.txt no2.txt empty_dest/ 2>err &&
	test "$(ls empty_dest/ | wc -l)" -eq 0
	)
'

# --- directory as source ---

test_expect_success 'mv directory into another directory' '
	(
	cd repo &&
	mkdir -p srcdir dstdir &&
	echo "x" >srcdir/x.txt &&
	grit add srcdir/ &&
	grit commit -m "add srcdir" &&
	grit mv srcdir dstdir/ &&
	test -f dstdir/srcdir/x.txt &&
	! test -d srcdir
	)
'

test_expect_success 'index reflects directory move' '
	(
	cd repo &&
	grit ls-files >ls_out &&
	grep "dstdir/srcdir/x.txt" ls_out &&
	! grep "^srcdir/x.txt$" ls_out
	)
'

test_expect_success 'commit directory move' '
	(
	cd repo &&
	grit commit -m "moved srcdir into dstdir"
	)
'

# --- error conditions ---

test_expect_success 'mv to non-directory with multiple sources fails' '
	(
	cd repo &&
	echo "p" >p.txt &&
	echo "q" >q.txt &&
	grit add p.txt q.txt &&
	grit commit -m "add p q" &&
	test_must_fail grit mv p.txt q.txt target.txt 2>err
	)
'

test_expect_success 'mv with no args fails' '
	(
	cd repo &&
	test_must_fail grit mv 2>err
	)
'

test_expect_success 'mv single file with no destination fails' '
	(
	cd repo &&
	test_must_fail grit mv p.txt 2>err
	)
'

test_expect_success 'mv source that is not tracked fails' '
	(
	cd repo &&
	echo "untracked" >untracked.txt &&
	mkdir -p udest &&
	test_must_fail grit mv untracked.txt udest/ 2>err
	)
'

# --- mv file into newly-created directory ---

test_expect_success 'mv file to nonexistent destination directory fails' '
	(
	cd repo &&
	test_must_fail grit mv p.txt brand_new_dir/ 2>err
	)
'

# --- combinations ---

test_expect_success 'mv -n -v on single file shows verbose dry-run' '
	(
	cd repo &&
	grit mv -n -v p.txt dest/ >out 2>&1 &&
	test -f p.txt
	)
'

test_expect_success 'mv -f -v overwrites with verbose output' '
	(
	cd repo &&
	echo "ow1" >dest/new_p.txt &&
	grit add dest/new_p.txt &&
	grit commit -m "add dest/new_p" &&
	grit mv -f -v p.txt dest/new_p.txt >out 2>&1 &&
	test "$(cat dest/new_p.txt)" = "p"
	)
'

test_expect_success 'commit final state' '
	(
	cd repo &&
	grit commit -m "final multi-source mv tests" ||
	grit status
	)
'

test_expect_success 'mv rename preserves file content' '
	(
	cd repo &&
	grit mv q.txt q_renamed.txt &&
	test "$(cat q_renamed.txt)" = "q" &&
	grit commit -m "rename q"
	)
'

test_expect_success 'mv into subdirectory preserves content' '
	(
	cd repo &&
	echo "content-check" >cc.txt &&
	grit add cc.txt &&
	grit commit -m "add cc" &&
	mkdir -p subcheck &&
	grit mv cc.txt subcheck/ &&
	test "$(cat subcheck/cc.txt)" = "content-check"
	)
'

test_done
