#!/bin/sh
# Test grit mv for files, directories, renames, -f, -n, -k, -v options.

test_description='grit mv directory rename'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with files and directories' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo alpha >alpha.txt &&
	echo beta >beta.txt &&
	mkdir -p src/lib &&
	echo main >src/main.c &&
	echo util >src/lib/util.c &&
	echo header >src/lib/util.h &&
	grit add . &&
	test_tick &&
	grit commit -m "initial"
	)
'

test_expect_success 'mv renames a single file' '
	(
	cd repo &&
	grit mv alpha.txt renamed.txt &&
	test_path_is_missing alpha.txt &&
	test_path_is_file renamed.txt &&
	cat renamed.txt | grep "alpha"
	)
'

test_expect_success 'mv rename shows in status as renamed' '
	(
	cd repo &&
	grit status --porcelain >status &&
	grep "renamed.txt" status
	)
'

test_expect_success 'mv commit records rename' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "rename alpha" &&
	grit ls-tree -r HEAD --name-only >files &&
	grep "renamed.txt" files &&
	! grep "alpha.txt" files
	)
'

test_expect_success 'mv file into directory' '
	(
	cd repo &&
	grit mv beta.txt src/ &&
	test_path_is_missing beta.txt &&
	test_path_is_file src/beta.txt
	)
'

test_expect_success 'mv file into directory preserves content' '
	(
	cd repo &&
	cat src/beta.txt | grep "beta"
	)
'

test_expect_success 'mv file into directory shows in status' '
	(
	cd repo &&
	grit status --porcelain >status &&
	grep "src/beta.txt" status
	)
'

test_expect_success 'mv commit file-into-dir and verify' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "move beta into src" &&
	grit ls-tree -r HEAD --name-only >files &&
	grep "src/beta.txt" files
	)
'

test_expect_success 'mv directory to new name' '
	(
	cd repo &&
	grit mv src pkg &&
	test_path_is_missing src &&
	test_path_is_dir pkg &&
	test_path_is_file pkg/main.c &&
	test_path_is_file pkg/lib/util.c
	)
'

test_expect_success 'mv directory rename shows in status' '
	(
	cd repo &&
	grit status --porcelain >status &&
	grep "pkg/main.c" status &&
	grep "pkg/lib/util.c" status
	)
'

test_expect_success 'mv commit directory rename' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "rename src to pkg" &&
	grit ls-tree -r HEAD --name-only >files &&
	grep "pkg/main.c" files &&
	grep "pkg/lib/util.c" files &&
	! grep "src/" files
	)
'

test_expect_success 'mv -v is verbose' '
	(
	cd repo &&
	echo extra >extra.txt &&
	grit add extra.txt &&
	test_tick &&
	grit commit -m "add extra" &&
	grit mv -v extra.txt moved-extra.txt >out 2>&1 &&
	grep "extra.txt" out
	)
'

test_expect_success 'mv -v commit verbose rename' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "move extra"
	)
'

test_expect_success 'mv -n dry-run does not move' '
	(
	cd repo &&
	grit mv -n moved-extra.txt dry-run-target.txt >out 2>&1 &&
	test_path_is_file moved-extra.txt &&
	test_path_is_missing dry-run-target.txt
	)
'

test_expect_success 'mv -n dry-run shows what would happen' '
	(
	cd repo &&
	grit mv -n moved-extra.txt dry-run-target.txt >out 2>&1 &&
	grep "moved-extra.txt" out
	)
'

test_expect_success 'mv -f forces overwrite of existing file' '
	(
	cd repo &&
	echo target >target.txt &&
	grit add target.txt &&
	test_tick &&
	grit commit -m "add target" &&
	grit mv -f moved-extra.txt target.txt &&
	test_path_is_missing moved-extra.txt &&
	cat target.txt | grep "extra"
	)
'

test_expect_success 'mv -f overwrite commit' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "force overwrite"
	)
'

test_expect_success 'mv multiple files into directory' '
	(
	cd repo &&
	echo one >one.txt &&
	echo two >two.txt &&
	grit add one.txt two.txt &&
	test_tick &&
	grit commit -m "add one two" &&
	mkdir dest &&
	grit mv one.txt two.txt dest/ &&
	test_path_is_missing one.txt &&
	test_path_is_missing two.txt &&
	test_path_is_file dest/one.txt &&
	test_path_is_file dest/two.txt
	)
'

test_expect_success 'mv multiple files commit' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "move one two into dest"
	)
'

test_expect_success 'mv to new path creates parent directories' '
	(
	cd repo &&
	echo fail >fail.txt &&
	grit add fail.txt &&
	test_tick &&
	grit commit -m "add fail" &&
	grit mv fail.txt sub-new/fail.txt &&
	test_path_is_file sub-new/fail.txt &&
	test_path_is_missing fail.txt
	)
'

test_expect_success 'mv to new path commit' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "move fail into sub-new" &&
	grit ls-tree -r HEAD --name-only >files &&
	grep "sub-new/fail.txt" files
	)
'

test_expect_success 'setup fresh repo for -k tests' '
	(
	rm -rf repo2 &&
	grit init repo2 &&
	cd repo2 &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo a >a.txt &&
	echo b >b.txt &&
	echo c >c.txt &&
	mkdir target &&
	grit add . &&
	test_tick &&
	grit commit -m "initial"
	)
'

test_expect_success 'mv -k does not abort on some failures' '
	(
	cd repo2 &&
	echo newfile >new.txt &&
	grit mv -k a.txt target/ 2>err;
	test_path_is_file target/a.txt
	)
'

test_expect_success 'mv file with spaces in name' '
	(
	cd repo2 &&
	echo spaced >"file with spaces.txt" &&
	grit add "file with spaces.txt" &&
	test_tick &&
	grit commit -m "add spaced file" &&
	grit mv "file with spaces.txt" "renamed spaces.txt" &&
	test_path_is_missing "file with spaces.txt" &&
	test_path_is_file "renamed spaces.txt"
	)
'

test_expect_success 'mv file with spaces commit' '
	(
	cd repo2 &&
	test_tick &&
	grit commit -m "rename spaced file"
	)
'

test_expect_success 'mv nested directory into another directory' '
	(
	cd repo2 &&
	mkdir -p deep/nested &&
	echo dn >deep/nested/file.txt &&
	grit add deep &&
	test_tick &&
	grit commit -m "add deep" &&
	grit mv deep target/ &&
	test_path_is_missing deep &&
	test_path_is_file target/deep/nested/file.txt
	)
'

test_expect_success 'mv nested directory commit' '
	(
	cd repo2 &&
	test_tick &&
	grit commit -m "move deep into target" &&
	grit ls-tree -r HEAD --name-only >files &&
	grep "target/deep/nested/file.txt" files
	)
'

test_expect_success 'mv preserves file content after rename' '
	(
	cd repo2 &&
	cat target/deep/nested/file.txt | grep "dn"
	)
'

test_expect_success 'mv -n -v shows verbose dry-run' '
	(
	cd repo2 &&
	grit mv -n -v b.txt z.txt >out 2>&1 &&
	test_path_is_file b.txt &&
	grep "b.txt" out
	)
'

test_expect_success 'mv untracked file fails' '
	(
	cd repo2 &&
	echo untracked >untracked.txt &&
	test_must_fail grit mv untracked.txt somewhere.txt 2>err
	)
'

test_expect_success 'mv directory rename preserves nested structure' '
	(
	cd repo2 &&
	grit mv target newdir &&
	test_path_is_file newdir/deep/nested/file.txt &&
	test_path_is_missing target
	)
'

test_expect_success 'mv directory rename commit' '
	(
	cd repo2 &&
	test_tick &&
	grit commit -m "rename target to newdir" &&
	grit ls-tree -r HEAD --name-only >files &&
	grep "newdir/deep/nested/file.txt" files &&
	! grep "^target/" files
	)
'

test_expect_success 'mv -f forces rename even if destination tracked' '
	(
	cd repo2 &&
	grit mv -f b.txt c.txt &&
	test_path_is_missing b.txt &&
	cat c.txt | grep "b"
	)
'

test_expect_success 'mv -f overwrite tracked commit' '
	(
	cd repo2 &&
	test_tick &&
	grit commit -m "force overwrite c with b" &&
	grit ls-tree -r HEAD --name-only >files &&
	grep "c.txt" files &&
	! grep "^b.txt" files
	)
'

test_done
