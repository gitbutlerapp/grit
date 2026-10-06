#!/bin/sh

test_description='grit mv: rename, move to directory, force, dry-run, verbose, case changes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
	 git config user.email "t@t.com" &&
	 git config user.name "T" &&
	 echo hello >file.txt &&
	 echo world >other.txt &&
	 mkdir -p subdir &&
	 echo nested >subdir/nested.txt &&
	 grit add . &&
	 grit commit -m "initial"
	)
'

test_expect_success 'mv renames file in index' '
	(cd repo &&
	 grit mv file.txt renamed.txt &&
	 grit ls-files --cached >../actual
	) &&
	grep "renamed.txt" actual &&
	! grep "^file.txt$" actual
'

test_expect_success 'mv renames file on disk' '
	test_path_is_file repo/renamed.txt &&
	test_path_is_missing repo/file.txt
'

test_expect_success 'mv preserves file content' '
	(cd repo &&
	 cat renamed.txt >../actual
	) &&
	echo hello >expect &&
	test_cmp expect actual
'

test_expect_success 'mv file into directory' '
	(cd repo &&
	 mkdir -p dest &&
	 grit mv other.txt dest/ &&
	 grit ls-files --cached >../actual
	) &&
	grep "dest/other.txt" actual &&
	! grep "^other.txt$" actual
'

test_expect_success 'mv file into directory preserves content' '
	(cd repo &&
	 cat dest/other.txt >../actual
	) &&
	echo world >expect &&
	test_cmp expect actual
'

test_expect_success 'mv --dry-run does not actually move' '
	(cd repo &&
	 grit mv --dry-run renamed.txt drytest.txt >../actual 2>&1
	) &&
	test_path_is_file repo/renamed.txt &&
	test_path_is_missing repo/drytest.txt &&
	grep "renamed.txt" actual
'

test_expect_success 'mv --verbose shows what is moved' '
	(cd repo &&
	 grit mv --verbose renamed.txt verbose-moved.txt >../actual 2>&1
	) &&
	grep "renamed.txt" actual
'

test_expect_success 'mv to existing file fails without --force' '
	(cd repo &&
	 echo existing >existing.txt &&
	 grit add existing.txt &&
	 grit commit -m "add existing" &&
	 echo target >target.txt &&
	 grit add target.txt &&
	 grit commit -m "add target" &&
	 test_must_fail grit mv target.txt existing.txt 2>../err
	) &&
	test -s err
'

test_expect_success 'mv --force overwrites existing file' '
	(cd repo &&
	 grit mv --force target.txt existing.txt &&
	 grit ls-files --cached >../actual
	) &&
	! grep "^target.txt$" actual &&
	grep "existing.txt" actual
'

test_expect_success 'mv --force overwrote content correctly' '
	(cd repo &&
	 cat existing.txt >../actual
	) &&
	echo target >expect &&
	test_cmp expect actual
'

test_expect_success 'mv changes case of filename' '
	(cd repo &&
	 echo casetest >lowercase.txt &&
	 grit add lowercase.txt &&
	 grit commit -m "add lowercase" &&
	 grit mv lowercase.txt LOWERCASE.txt &&
	 grit ls-files --cached >../actual
	) &&
	grep "LOWERCASE.txt" actual
'

test_expect_success 'mv executable file preserves mode' '
	(cd repo &&
	 echo "#!/bin/sh" >exec.sh &&
	 chmod +x exec.sh &&
	 grit add exec.sh &&
	 grit commit -m "add exec" &&
	 grit mv exec.sh moved-exec.sh &&
	 grit ls-files --stage moved-exec.sh >../actual
	) &&
	grep "^100755" actual
'

test_expect_success 'mv file with spaces' '
	(cd repo &&
	 echo spaced >"file with spaces.txt" &&
	 grit add "file with spaces.txt" &&
	 grit commit -m "add spaced" &&
	 grit mv "file with spaces.txt" "new name.txt" &&
	 grit ls-files --cached >../actual
	) &&
	grep "new name.txt" actual &&
	! grep "file with spaces.txt" actual
'

test_expect_success 'mv into new subdirectory' '
	(cd repo &&
	 echo moveme >moveme.txt &&
	 grit add moveme.txt &&
	 grit commit -m "add moveme" &&
	 mkdir -p newdir &&
	 grit mv moveme.txt newdir/ &&
	 grit ls-files --cached >../actual
	) &&
	grep "newdir/moveme.txt" actual
'

test_expect_success 'mv then commit records rename' '
	(cd repo &&
	 echo forcommit >precommit.txt &&
	 grit add precommit.txt &&
	 grit commit -m "add precommit" &&
	 grit mv precommit.txt postcommit.txt &&
	 grit commit -m "rename precommit" &&
	 grit ls-tree HEAD >../actual
	) &&
	grep "postcommit.txt" actual &&
	! grep "precommit.txt" actual
'

test_expect_success 'mv multiple files into directory' '
	(cd repo &&
	 echo m1 >m1.txt &&
	 echo m2 >m2.txt &&
	 grit add m1.txt m2.txt &&
	 grit commit -m "add m1 m2" &&
	 mkdir -p multi-dest &&
	 grit mv m1.txt m2.txt multi-dest/ &&
	 grit ls-files --cached >../actual
	) &&
	grep "multi-dest/m1.txt" actual &&
	grep "multi-dest/m2.txt" actual
'

test_expect_success 'mv untracked file fails' '
	(cd repo &&
	 echo untracked >untracked.txt &&
	 test_must_fail grit mv untracked.txt somewhere.txt 2>../err
	) &&
	test -s err
'

test_expect_success 'mv nonexistent file fails' '
	(cd repo &&
	 test_must_fail grit mv nosuchfile.txt dst.txt 2>../err
	) &&
	test -s err
'

test_expect_success 'mv -k skips errors instead of aborting' '
	(cd repo &&
	 echo valid >valid.txt &&
	 grit add valid.txt &&
	 grit commit -m "add valid" &&
	 mkdir -p kdir &&
	 grit mv -k nosuchfile.txt valid.txt kdir/ 2>../err &&
	 grit ls-files --cached >../actual
	) &&
	grep "kdir/valid.txt" actual
'

test_expect_success 'mv empty file' '
	(cd repo &&
	 : >empty.txt &&
	 grit add empty.txt &&
	 grit commit -m "add empty" &&
	 grit mv empty.txt empty-moved.txt &&
	 grit ls-files --cached >../actual
	) &&
	grep "empty-moved.txt" actual
'

test_expect_success 'mv nested file up a level' '
	(cd repo &&
	 mkdir -p deep/nest &&
	 echo deepf >deep/nest/f.txt &&
	 grit add deep &&
	 grit commit -m "add deep" &&
	 grit mv deep/nest/f.txt deep/f.txt &&
	 grit ls-files --cached >../actual
	) &&
	grep "deep/f.txt" actual &&
	! grep "deep/nest/f.txt" actual
'

test_expect_success 'mv file to itself fails' '
	(cd repo &&
	 test_must_fail grit mv deep/f.txt deep/f.txt 2>../err
	) &&
	test -s err
'

test_expect_success 'mv preserves symlink' '
	(cd repo &&
	 echo linktarget >lt.txt &&
	 grit add lt.txt &&
	 ln -sf lt.txt mylink &&
	 grit add mylink &&
	 grit commit -m "add link" &&
	 grit mv mylink moved-link &&
	 grit ls-files --stage moved-link >../actual
	) &&
	grep "120000" actual
'

test_expect_success 'mv --dry-run --verbose shows planned action' '
	(cd repo &&
	 echo dryv >dryv.txt &&
	 grit add dryv.txt &&
	 grit commit -m "add dryv" &&
	 grit mv --dry-run --verbose dryv.txt dryv-moved.txt >../actual 2>&1
	) &&
	grep "dryv.txt" actual &&
	test_path_is_file repo/dryv.txt
'

test_expect_success 'mv file then status shows rename' '
	(cd repo &&
	 grit mv dryv.txt dryv-moved.txt &&
	 grit status >../actual 2>&1
	) &&
	grep "dryv" actual
'

test_expect_success 'mv directory rename' '
	(cd repo &&
	 mkdir -p olddir &&
	 echo od1 >olddir/one.txt &&
	 echo od2 >olddir/two.txt &&
	 grit add olddir &&
	 grit commit -m "add olddir" &&
	 grit mv olddir newold &&
	 grit ls-files --cached >../actual
	) &&
	grep "newold/one.txt" actual &&
	grep "newold/two.txt" actual &&
	! grep "olddir/" actual
'

test_expect_success 'mv file across directories' '
	(cd repo &&
	 mkdir -p srcdir dstdir &&
	 echo cross >srcdir/cross.txt &&
	 grit add srcdir &&
	 grit commit -m "add srcdir" &&
	 grit mv srcdir/cross.txt dstdir/ &&
	 grit ls-files --cached >../actual
	) &&
	grep "dstdir/cross.txt" actual &&
	! grep "srcdir/cross.txt" actual
'

test_expect_success 'mv add extension to file' '
	(cd repo &&
	 echo noext >noext &&
	 grit add noext &&
	 grit commit -m "add noext" &&
	 grit mv noext noext.txt &&
	 grit ls-files --cached >../actual
	) &&
	grep "noext.txt" actual &&
	! grep "^noext$" actual
'

test_expect_success 'mv remove extension from file' '
	(cd repo &&
	 grit mv noext.txt noext &&
	 grit ls-files --cached >../actual
	) &&
	grep "^noext$" actual
'

test_done
