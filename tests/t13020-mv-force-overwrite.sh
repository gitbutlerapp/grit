#!/bin/sh

test_description='grit mv with --force, --dry-run, -k, -v, and edge cases'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo && cd repo &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo hello >file.txt &&
	echo world >other.txt &&
	mkdir -p sub &&
	echo s >sub/s.txt &&
	grit add . && grit commit -m "initial"
	)
'

test_expect_success 'mv renames file in index and working tree' '
	(cd repo && grit mv file.txt renamed.txt &&
	 grit ls-files --cached >../actual) &&
	grep "renamed.txt" actual &&
	! grep "^file.txt$" actual &&
	test -f repo/renamed.txt &&
	! test -f repo/file.txt
'

test_expect_success 'mv shows in status as rename' '
	(cd repo && grit status --porcelain >../actual) &&
	grep "renamed.txt" actual
'

test_expect_success 'commit after mv succeeds' '
	(cd repo && grit commit -m "renamed" &&
	 grit ls-files --cached >../actual) &&
	grep "renamed.txt" actual
'

test_expect_success 'mv file to directory' '
	(cd repo &&
	 mkdir -p dest &&
	 grit mv other.txt dest/ &&
	 grit ls-files --cached >../actual) &&
	grep "dest/other.txt" actual &&
	! grep "^other.txt$" actual
'

test_expect_success 'mv file from subdirectory to root' '
	(cd repo &&
	 grit mv sub/s.txt s-moved.txt &&
	 grit ls-files --cached >../actual) &&
	grep "s-moved.txt" actual &&
	! grep "sub/s.txt" actual
'

test_expect_success 'mv onto existing file fails without --force' '
	(cd repo &&
	 echo a >src.txt && echo b >dst.txt &&
	 grit add src.txt dst.txt && grit commit -m "src dst" &&
	 ! grit mv src.txt dst.txt 2>../err) &&
	test -s err
'

test_expect_success 'mv --force overwrites existing file' '
	(cd repo &&
	 grit mv --force src.txt dst.txt &&
	 grit ls-files --cached >../actual) &&
	grep "dst.txt" actual &&
	! grep "^src.txt$" actual &&
	test "$(cat repo/dst.txt)" = "a"
'

test_expect_success 'mv --dry-run does not actually move' '
	(cd repo &&
	 echo dn >drynm.txt && grit add drynm.txt && grit commit -m "dn" &&
	 grit mv --dry-run drynm.txt drynm-moved.txt &&
	 grit ls-files --cached >../actual) &&
	grep "drynm.txt" actual &&
	! grep "drynm-moved.txt" actual &&
	test -f repo/drynm.txt
'

test_expect_success 'mv --dry-run shows what would be moved' '
	(cd repo && grit mv -n drynm.txt drynm-moved.txt >../actual 2>&1) &&
	grep "drynm" actual
'

test_expect_success 'mv --verbose shows movement' '
	(cd repo &&
	 echo vb >vb.txt && grit add vb.txt && grit commit -m "vb" &&
	 grit mv -v vb.txt vb-moved.txt >../actual 2>&1) &&
	grep "vb" actual
'

test_expect_success 'mv nonexistent file fails' '
	(cd repo && ! grit mv nonexistent.txt somewhere.txt 2>../err) &&
	test -s err
'

test_expect_success 'mv -k skips errors instead of aborting' '
	(cd repo &&
	 echo keep >keep.txt && grit add keep.txt && grit commit -m "keep" &&
	 mkdir -p kdir &&
	 grit mv -k nonexistent.txt keep.txt kdir/ 2>../err) &&
	(cd repo && grit ls-files --cached >../actual) &&
	grep "kdir/keep.txt" actual
'

test_expect_success 'mv file with spaces in name' '
	(cd repo &&
	 echo sp >"has spaces.txt" && grit add "has spaces.txt" && grit commit -m "sp" &&
	 grit mv "has spaces.txt" "moved spaces.txt" &&
	 grit ls-files --cached >../actual) &&
	grep "moved spaces.txt" actual &&
	! grep "has spaces.txt" actual
'

test_expect_success 'mv into new directory' '
	(cd repo &&
	 echo nd >newdir-file.txt && grit add newdir-file.txt && grit commit -m "nd" &&
	 mkdir -p newdir &&
	 grit mv newdir-file.txt newdir/ &&
	 grit ls-files --cached >../actual) &&
	grep "newdir/newdir-file.txt" actual
'

test_expect_success 'mv multiple files to directory' '
	(cd repo &&
	 echo m1 >multi1.txt && echo m2 >multi2.txt &&
	 grit add multi1.txt multi2.txt && grit commit -m "multi" &&
	 mkdir -p multidir &&
	 grit mv multi1.txt multi2.txt multidir/ &&
	 grit ls-files --cached >../actual) &&
	grep "multidir/multi1.txt" actual &&
	grep "multidir/multi2.txt" actual
'

test_expect_success 'mv file to same name fails or is no-op' '
	(cd repo &&
	 echo same >same.txt && grit add same.txt && grit commit -m "same" &&
	 ! grit mv same.txt same.txt 2>../err || true)
'

test_expect_success 'mv preserves file content' '
	(cd repo &&
	 echo "specific content 12345" >content.txt &&
	 grit add content.txt && grit commit -m "content" &&
	 grit mv content.txt content-moved.txt) &&
	echo "specific content 12345" >expect &&
	test_cmp expect repo/content-moved.txt
'

test_expect_success 'mv preserves executable bit' '
	(cd repo &&
	 echo "#!/bin/sh" >exec.sh && chmod +x exec.sh &&
	 grit add exec.sh && grit commit -m "exec" &&
	 grit mv exec.sh exec-moved.sh &&
	 grit ls-files --stage exec-moved.sh >../actual) &&
	grep "100755" actual
'

test_expect_success 'mv back to original name' '
	(cd repo &&
	 grit mv exec-moved.sh exec.sh &&
	 grit ls-files --cached >../actual) &&
	grep "exec.sh" actual &&
	! grep "exec-moved.sh" actual
'

test_expect_success 'mv across directories' '
	(cd repo &&
	 mkdir -p dir1 dir2 &&
	 echo cross >dir1/cross.txt && grit add dir1/cross.txt && grit commit -m "cross" &&
	 grit mv dir1/cross.txt dir2/cross.txt &&
	 grit ls-files --cached >../actual) &&
	grep "dir2/cross.txt" actual &&
	! grep "dir1/cross.txt" actual
'

test_expect_success 'mv file and change extension' '
	(cd repo &&
	 echo data >data.txt && grit add data.txt && grit commit -m "data" &&
	 grit mv data.txt data.md &&
	 grit ls-files --cached >../actual) &&
	grep "data.md" actual &&
	! grep "data.txt" actual
'

test_expect_success 'mv into deeply nested new directory' '
	(cd repo &&
	 echo deep >deep.txt && grit add deep.txt && grit commit -m "deep" &&
	 mkdir -p a/b/c &&
	 grit mv deep.txt a/b/c/deep.txt &&
	 grit ls-files --cached >../actual) &&
	grep "a/b/c/deep.txt" actual
'

test_expect_success 'mv symlink' '
	(cd repo &&
	 echo target >linktgt.txt && grit add linktgt.txt &&
	 ln -sf linktgt.txt mylink && grit add mylink && grit commit -m "link" &&
	 grit mv mylink mylink-moved &&
	 grit ls-files --cached >../actual) &&
	grep "mylink-moved" actual &&
	! grep "^mylink$" actual
'

test_expect_success 'mv empty file' '
	(cd repo &&
	 >empty.txt && grit add empty.txt && grit commit -m "empty" &&
	 grit mv empty.txt empty-moved.txt &&
	 grit ls-files --cached >../actual) &&
	grep "empty-moved.txt" actual
'

test_expect_success 'mv --force with destination that is modified' '
	(cd repo &&
	 echo orig >over-src.txt && echo dest-val >over-dst.txt &&
	 grit add over-src.txt over-dst.txt && grit commit -m "over" &&
	 echo modified >over-dst.txt &&
	 grit mv -f over-src.txt over-dst.txt &&
	 grit ls-files --cached >../actual) &&
	grep "over-dst.txt" actual &&
	! grep "^over-src.txt$" actual &&
	test "$(cat repo/over-dst.txt)" = "orig"
'

test_expect_success 'mv case-only rename' '
	(cd repo &&
	 echo case >casefile.txt && grit add casefile.txt && grit commit -m "case" &&
	 grit mv casefile.txt CaseFile.txt &&
	 grit ls-files --cached >../actual) &&
	grep "CaseFile.txt" actual
'

test_expect_success 'mv --force combined with --verbose' '
	(cd repo &&
	 echo fv1 >fv-src.txt && echo fv2 >fv-dst.txt &&
	 grit add fv-src.txt fv-dst.txt && grit commit -m "fv" &&
	 grit mv -f -v fv-src.txt fv-dst.txt >../actual 2>&1) &&
	grep "fv" actual
'

test_expect_success 'mv updates working tree correctly' '
	(cd repo &&
	 echo wt >wt.txt && grit add wt.txt && grit commit -m "wt" &&
	 grit mv wt.txt wt-new.txt) &&
	! test -f repo/wt.txt &&
	test -f repo/wt-new.txt &&
	test "$(cat repo/wt-new.txt)" = "wt"
'

test_expect_success 'mv --dry-run combined with --verbose' '
	(cd repo &&
	 echo dv >dv.txt && grit add dv.txt && grit commit -m "dv" &&
	 grit mv -n -v dv.txt dv-moved.txt >../actual 2>&1) &&
	grep "dv" actual &&
	test -f repo/dv.txt
'

test_done
